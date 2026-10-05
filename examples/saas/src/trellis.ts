// What Trellis does, apart from HTTP: accounts, sessions, organisations,
// invitations and members. Every statement runs through the router with a
// scoped token -- the app's own for the auth flows, the person's for what
// an admin changes in their organisation -- so the policy, not this file,
// is what decides who may do what. What this file adds is the part a
// database cannot: hashing, mail, and the order of the steps.
import { DbError, Db } from './db.ts';
import { type Config, ACCOUNTS_SCHEMA, ORG_SCHEMA } from './config.ts';
import { signJwt, verifyJwt, type Jwk, type SigningKey } from './jwt.ts';
import { checkPassword, digest, hashPassword, newId, normaliseEmail, passwordProblem, secret } from './passwords.ts';
import { ISSUER, ROLES, Tokens, type Role } from './tokens.ts';

export interface Reply {
  status: number;
  body: Record<string, unknown>;
  /** A refresh token to set as the cookie, or null to clear it. */
  refresh?: string | null;
}

const reply = (status: number, body: Record<string, unknown>, refresh?: string | null): Reply => ({ status, body, refresh });

/** What sign-in answers for every failure that is not a rate limit, whether or not the email exists. */
export const WRONG = 'The email or password is not right.';
export const LIMITED = 'Too many attempts. Wait ten minutes, or reset your password.';

export interface User {
  uid: string;
  email: string;
  name: string;
  verified: boolean;
}

export interface Caller {
  sub: string;
  name: string;
  /** For an organisation's token: its slug, role and teams. */
  org?: string;
  role?: Role;
  teams?: string[];
  token: string;
}

export const tenantOf = (slug: string) => `o-${slug}`;
const SLUG = /^[a-z0-9](?:[a-z0-9-]{1,38}[a-z0-9])$/;
const RESERVED = new Set(['accounts', 'admin', 'api', 'new', 'www', 'trellis', 'system']);

export class Trellis {
  readonly tokens: Tokens;
  readonly accounts: Db;
  #publicKeys: Jwk[];
  /** A dummy hash, checked when the email is unknown so both cases take one scrypt. */
  #dummy: Promise<string>;

  constructor(
    readonly cfg: Config,
    signing: SigningKey,
    publicKeys: Jwk[],
  ) {
    this.tokens = new Tokens(signing, cfg.accessTtl);
    this.#publicKeys = publicKeys;
    this.accounts = new Db(`${cfg.routerUrl}/t/${cfg.accountsTenant}`, this.tokens.app(cfg.accountsTenant));
    this.#dummy = hashPassword('not anyone\'s password, ever', cfg.scryptN);
  }

  /** The app's view of the accounts tenant (its token renewed as it ages). */
  #acc(): Db {
    const t = this.tokens.app(this.cfg.accountsTenant);
    return t === this.accounts.token ? this.accounts : new Db(this.accounts.base, t);
  }

  /** The app's view of an organisation. */
  org(slug: string): Db {
    const tenant = tenantOf(slug);
    return new Db(`${this.cfg.routerUrl}/t/${tenant}`, this.tokens.app(tenant));
  }

  /** An organisation as a person sees it: their own token, the policy's rules. */
  orgAs(slug: string, token: string): Db {
    return new Db(`${this.cfg.routerUrl}/t/${tenantOf(slug)}`, token);
  }

  // ------------------------------------------------------------------ setup

  /** The accounts tenant placed and given its schema; safe to run again. */
  async provision(): Promise<void> {
    await this.#placeTenant(this.cfg.accountsTenant, ACCOUNTS_SCHEMA);
  }

  async #placeTenant(tenant: string, schema: string): Promise<void> {
    // The router places the tenant and applies its schema there, with the
    // nodes' admin tokens it holds: this app needs the router's token alone,
    // where it held the nodes' data token, which reaches every tenant, to
    // apply the schema itself. A tenant is there with its schema or not at
    // all; one that exists (409) keeps the one it was made with, and a
    // change to it is a migration the operator runs.
    const put = await fetch(`${this.cfg.routerUrl}/_shard/tenants/${tenant}`, {
      method: 'PUT',
      headers: { authorization: `Bearer ${this.cfg.shardToken}`, 'content-type': 'application/json' },
      body: JSON.stringify({ schema }),
    });
    if (!put.ok && put.status !== 409) throw new Error(`place ${tenant}: ${put.status} ${await put.text()}`);
  }

  // ------------------------------------------------------------- the tokens

  /**
   * A token this app signed, checked as fenec-server checks it: RS256
   * against the public key, `exp` required. `tenant` holds an organisation's
   * token to its organisation; an API token names none.
   */
  caller(header: string | undefined, slug?: string): Caller | null {
    const token = /^Bearer (.+)$/.exec(header ?? '')?.[1];
    if (!token) return null;
    let c: Record<string, unknown>;
    try {
      c = verifyJwt(token, this.#publicKeys, { iss: ISSUER, leeway: 0 });
    } catch {
      return null;
    }
    if (typeof c.sub !== 'string' || c.sub === 'trellis-app') return null;
    if (slug === undefined) {
      if (c.tenant !== undefined || c.aud !== 'trellis-api') return null;
      return { sub: c.sub, name: String(c.name ?? ''), token };
    }
    if (c.tenant !== tenantOf(slug)) return null;
    const roles = Array.isArray(c.role) ? (c.role as Role[]) : [];
    return {
      sub: c.sub,
      name: String(c.name ?? ''),
      org: slug,
      role: ROLES.find((r) => roles.includes(r)),
      teams: Array.isArray(c.teams) ? (c.teams as string[]) : [],
      token,
    };
  }

  // --------------------------------------------------------------- accounts

  /**
   * Sign up. The answer is the same whether or not the email has an
   * account, so the form cannot be used to find out who has one: an email
   * already taken is told so in the inbox, not on the page.
   */
  async signUp(input: { email: string; name: string; password: string }, ip: string): Promise<Reply> {
    const email = normaliseEmail(input.email);
    if (!email) return reply(400, { error: 'Enter an email address like name@example.com.' });
    const name = input.name.trim().slice(0, 80);
    if (!name) return reply(400, { error: 'Enter your name.' });
    const bad = passwordProblem(input.password);
    if (bad) return reply(400, { error: bad });
    if (!(await this.#count(`signup:${ip}`, this.cfg.ipLimit))) return reply(429, { error: LIMITED });

    const password = await hashPassword(input.password, this.cfg.scryptN);
    const uid = newId('u');
    const acc = this.#acc();
    const made = await acc.batch([
      ['put users {uid: $1, email: $2, name: $3, password: $4, verified: false, created: now()} if absent', [uid, email, name, password]],
    ]);
    if (!made.ok) throw new Error(made.error);
    if (made.results[0].affected === 1) {
      await this.#mailVerification(uid, email, name, ip);
    } else {
      await this.#mail(email, 'Someone tried to sign up with your email', [
        'Someone tried to make a Trellis account with this address, which already has one.',
        `If it was you, sign in at ${this.cfg.origin}/signin, or reset your password at ${this.cfg.origin}/forgot.`,
      ]);
    }
    return reply(202, { message: 'Check your inbox for a link to confirm your address.' });
  }

  async #mailVerification(uid: string, email: string, name: string, ip: string): Promise<void> {
    const code = secret();
    const out = await this.#acc().batch([
      ['insert verifications {hash: $1, user: $2, at: now()}', [digest(code), uid]],
      ['insert security_log {user: $1, kind: "signup", ip: $2, detail: $3, at: now()}', [uid, ip, email]],
    ]);
    if (!out.ok) throw new Error(out.error);
    await this.#mail(email, 'Confirm your email for Trellis', [
      `Hello ${name},`,
      `Confirm this address to create or join an organisation: ${this.cfg.origin}/verify?token=${code}`,
      'The link works once, for 24 hours.',
    ]);
  }

  /**
   * The counter recipe, in one block: the window's row made if absent, then
   * incremented only while under the limit -- `require 1` turns "already at
   * the limit" into a refusal that puts the block back.
   */
  async #count(key: string, limit: number): Promise<boolean> {
    const out = await this.#acc().batch([
      ['put limits {key: $1, n: 0, at: now()} if absent', [key]],
      ['set limits {n: n + 1} where key = $1 and n < $2 require 1', [key, limit]],
    ]);
    if (out.ok) return true;
    if (out.status === 412) return false;
    throw new Error(out.error);
  }

  /**
   * Sign in: both rate limits and the account's row in one block, the
   * password checked against the row's hash or a dummy one, so an unknown
   * email takes as long and answers the same as a wrong password.
   */
  async signIn(input: { email: string; password: string }, ip: string, agent: string): Promise<Reply> {
    const email = normaliseEmail(input.email) ?? input.email.slice(0, 254);
    const ipKey = `ip:${ip}`;
    const acctKey = `acct:${digest(email)}`;
    const out = await this.#acc().batch([
      ['put limits {key: $1, n: 0, at: now()} if absent', [ipKey]],
      ['set limits {n: n + 1} where key = $1 and n < $2 require 1', [ipKey, this.cfg.ipLimit]],
      ['put limits {key: $1, n: 0, at: now()} if absent', [acctKey]],
      ['set limits {n: n + 1} where key = $1 and n < $2 require 1', [acctKey, this.cfg.accountLimit]],
      ['get users select uid, email, name, password, verified where email = $1 limit 1', [email]],
    ]);
    if (!out.ok) {
      if (out.status === 412) {
        await this.#log('', 'signin.limited', ip, email);
        return reply(429, { error: LIMITED });
      }
      throw new Error(out.error);
    }
    const u = out.results[4].rows?.[0] as (User & { password: string }) | undefined;
    const ok = await checkPassword(input.password, u?.password || (await this.#dummy));
    if (!u || !ok) {
      await this.#log(u?.uid ?? '', 'signin.failed', ip, email);
      return reply(401, { error: WRONG });
    }
    return this.#startSession(u, ip, agent, 'signin', [['del limits where key = $1', [acctKey]]]);
  }

  /** A new refresh family for an account, and the API token for this tab. */
  async #startSession(u: User, ip: string, agent: string, kind: string, also: [string, unknown[]][] = []): Promise<Reply> {
    const rt = secret();
    const out = await this.#acc().batch([
      ...also,
      [
        'insert refresh {hash: $1, family: $2, user: $3, used: false, revoked: false, agent: $4, at: now()}',
        [digest(rt), newId('f'), u.uid, agent.slice(0, 200)],
      ],
      ['insert security_log {user: $1, kind: $2, ip: $3, detail: $4, at: now()}', [u.uid, kind, ip, agent.slice(0, 200)]],
    ]);
    if (!out.ok) throw new Error(out.error);
    return reply(200, await this.#session(u), rt);
  }

  /** What a page needs after a sign-in or a refresh: who, and in which organisations. */
  async #session(u: User): Promise<Record<string, unknown>> {
    const orgs = await this.#acc().rows<{ org: string }>('get memberships select org where user = $1 order org limit 200', [u.uid]);
    const named = orgs.length
      ? await this.#acc().rows<{ slug: string; name: string }>(
          // A parameter binds one value, so `in` takes one a slug.
          `get orgs select slug, name where slug in [${orgs.map((_, i) => `$${i + 1}`).join(', ')}] order name`,
          orgs.map((o) => o.org),
        )
      : [];
    return {
      user: { uid: u.uid, email: u.email, name: u.name, verified: u.verified },
      orgs: named.map((o) => ({ slug: o.slug, name: o.name })),
      api: this.#apiToken(u),
    };
  }

  /** A token for this app's own API, naming no tenant: fenec-server refuses it everywhere. */
  #apiToken(u: User): { token: string; exp: number } {
    const now = Math.floor(Date.now() / 1000);
    const exp = now + this.cfg.accessTtl;
    return { token: this.#sign({ iss: ISSUER, aud: 'trellis-api', sub: u.uid, name: u.name, iat: now, exp }), exp };
  }

  #sign(claims: Record<string, unknown>): string {
    return signJwt(claims, this.tokens.signing);
  }

  /**
   * A refresh token used, and the next one of its family given back.
   * Marking the old one used is conditional -- `where used = false ...
   * require 1` -- and in one block with the insert of the next, so two
   * requests racing with one token cannot both rotate it. A token seen
   * used again is a stolen copy, or the original after a thief rotated
   * it: the whole family is revoked, and both lose the session.
   */
  async refresh(rt: string | undefined, ip: string, agent: string, slug?: string): Promise<Reply> {
    if (!rt) return reply(401, { error: 'Sign in to continue.' }, null);
    const acc = this.#acc();
    const hash = digest(rt);
    const row = await acc.one<{ family: string; user: string; used: boolean; revoked: boolean }>(
      'get refresh select family, user, used, revoked where hash = $1 limit 1',
      [hash],
    );
    if (!row) return reply(401, { error: 'Your session has ended. Sign in again.' }, null);
    if (row.used || row.revoked) {
      if (row.used && !row.revoked) await this.#revokeFamily(row.family, row.user, ip, 'refresh.reuse');
      return reply(401, { error: 'Your session has ended. Sign in again.' }, null);
    }
    const next = secret();
    const out = await acc.batch([
      ['set refresh {used: true} where hash = $1 and used = false and revoked = false require 1', [hash]],
      [
        'insert refresh {hash: $1, family: $2, user: $3, used: false, revoked: false, agent: $4, at: now()}',
        [digest(next), row.family, row.user, agent.slice(0, 200)],
      ],
      ['get users select uid, email, name, verified where uid = $1 limit 1 require 1', [row.user]],
    ]);
    if (!out.ok) {
      if (out.status !== 412) throw new Error(out.error);
      // Another request rotated it first: one of the two is a copy.
      if (out.at === 0) await this.#revokeFamily(row.family, row.user, ip, 'refresh.reuse');
      return reply(401, { error: 'Your session has ended. Sign in again.' }, null);
    }
    const u = out.results[2].rows![0] as unknown as User;
    const body = await this.#session(u);
    if (slug !== undefined) {
      const access = await this.access(u, slug);
      if (!access) return reply(403, { ...body, error: 'You are not a member of that organisation.' }, next);
      body.access = access;
    }
    return reply(200, body, next);
  }

  async #revokeFamily(family: string, user: string, ip: string, kind: string): Promise<void> {
    const out = await this.#acc().batch([
      ['set refresh {revoked: true} where family = $1', [family]],
      ['insert security_log {user: $1, kind: $2, ip: $3, detail: $4, at: now()}', [user, kind, ip, family]],
    ]);
    if (!out.ok) throw new Error(out.error);
  }

  async signOut(rt: string | undefined, ip: string): Promise<Reply> {
    if (rt) {
      const row = await this.#acc().one<{ family: string; user: string }>('get refresh select family, user where hash = $1 limit 1', [digest(rt)]);
      if (row) await this.#revokeFamily(row.family, row.user, ip, 'signout');
    }
    return reply(200, { message: 'Signed out.' }, null);
  }

  /**
   * An organisation's access token, its role and teams read from the
   * organisation's own `members` at this moment: a role changed or a team
   * left counts from the next refresh, at most five minutes later.
   */
  async access(u: { uid: string; name: string }, slug: string): Promise<Record<string, unknown> | null> {
    if (!SLUG.test(slug)) return null;
    const org = this.org(slug);
    let m: { role: Role; teams: string[] | null; name: string } | undefined;
    try {
      m = await org.one('get members select role, teams, name where user = $1 limit 1', [u.uid]);
    } catch (e) {
      if (e instanceof DbError && (e.status === 404 || e.status === 403)) return null;
      throw e;
    }
    if (!m || !ROLES.includes(m.role)) return null;
    let teams = m.teams ?? [];
    // An admin reads every team (policy.txt); the claim lists them all so
    // the board can show them.
    if (m.role === 'owner' || m.role === 'admin') {
      teams = (await org.rows<{ key: string }>('get teams select key limit 500')).map((t) => t.key);
    }
    const a = this.tokens.user({ sub: u.uid, tenant: tenantOf(slug), role: m.role, teams, name: m.name || u.name });
    return {
      token: a.token,
      exp: a.exp,
      org: slug,
      tenant: tenantOf(slug),
      role: m.role,
      teams,
      db: `/db/t/${tenantOf(slug)}`,
    };
  }

  async verifyEmail(code: string, ip: string): Promise<Reply> {
    const acc = this.#acc();
    const row = await acc.one<{ user: string }>('get verifications select user where hash = $1 limit 1', [digest(code)]);
    if (!row) return reply(400, { error: 'This link has expired or was already used.' });
    // `del ... require 1`: the link works once, and a lapsed row is out of
    // the delete's reach as of every read, so it fails the same way.
    const out = await acc.batch([
      ['del verifications where hash = $1 require 1', [digest(code)]],
      ['set users {verified: true} where uid = $1 require 1', [row.user]],
      ['insert security_log {user: $1, kind: "verified", ip: $2, detail: "", at: now()}', [row.user, ip]],
    ]);
    if (!out.ok) {
      if (out.status === 412) return reply(400, { error: 'This link has expired or was already used.' });
      throw new Error(out.error);
    }
    return reply(200, { message: 'Your email is confirmed.' });
  }

  /** Always the same answer: whether an account exists is the inbox's to know. */
  async forgot(rawEmail: string, ip: string): Promise<Reply> {
    const email = normaliseEmail(rawEmail);
    const answer = reply(202, { message: 'If that address has an account, a reset link is on its way.' });
    if (!email) return answer;
    if (!(await this.#count(`forgot:${ip}`, this.cfg.ipLimit)) || !(await this.#count(`forgot:${digest(email)}`, 3))) {
      return answer;
    }
    const u = await this.#acc().one<User>('get users select uid, name where email = $1 limit 1', [email]);
    if (!u) return answer;
    const code = secret();
    const out = await this.#acc().batch([
      ['insert resets {hash: $1, user: $2, at: now()}', [digest(code), u.uid]],
      ['insert security_log {user: $1, kind: "reset.requested", ip: $2, detail: "", at: now()}', [u.uid, ip]],
    ]);
    if (!out.ok) throw new Error(out.error);
    await this.#mail(email, 'Reset your Trellis password', [
      `Hello ${u.name},`,
      `Choose a new password here: ${this.cfg.origin}/reset?token=${code}`,
      'The link works once, for 30 minutes. If you did not ask for it, ignore this mail.',
    ]);
    return answer;
  }

  /**
   * A new password, from a reset link: the link taken (`del ... require 1`,
   * once and only while it lives), the password set and every session of
   * the account revoked, in one block.
   */
  async reset(code: string, password: string, ip: string): Promise<Reply> {
    const bad = passwordProblem(password);
    if (bad) return reply(400, { error: bad });
    const acc = this.#acc();
    const row = await acc.one<{ user: string }>('get resets select user where hash = $1 limit 1', [digest(code)]);
    if (!row) return reply(400, { error: 'This link has expired or was already used.' });
    const hashed = await hashPassword(password, this.cfg.scryptN);
    const out = await acc.batch([
      ['del resets where hash = $1 require 1', [digest(code)]],
      ['set users {password: $2} where uid = $1 require 1', [row.user, hashed]],
      ['set refresh {revoked: true} where user = $1 and revoked = false', [row.user]],
      ['insert security_log {user: $1, kind: "reset", ip: $2, detail: "", at: now()}', [row.user, ip]],
    ]);
    if (!out.ok) {
      if (out.status === 412) return reply(400, { error: 'This link has expired or was already used.' });
      throw new Error(out.error);
    }
    return reply(200, { message: 'Your password is changed, and every other session is signed out.' });
  }

  /**
   * A sign-in through the identity provider, whose ID token the caller has
   * verified. The provider's subject finds the account; failing that, an
   * address the provider says it verified finds or makes one.
   */
  async signInWithIdentity(id: { issuer: string; sub: string; email: string; verified: boolean; name: string }, ip: string, agent: string): Promise<Reply> {
    const acc = this.#acc();
    const subject = `${id.issuer}#${id.sub}`;
    const link = await acc.one<{ user: string }>('get identities select user where subject = $1 limit 1', [subject]);
    let u: User | undefined;
    if (link) {
      u = await acc.one<User>('get users select uid, email, name, verified where uid = $1 limit 1', [link.user]);
    } else {
      const email = normaliseEmail(id.email);
      if (!email || !id.verified) return reply(403, { error: 'The identity provider did not confirm that email address.' });
      u = await acc.one<User>('get users select uid, email, name, verified where email = $1 limit 1', [email]);
      const statements: [string, unknown[]][] = [];
      if (!u) {
        u = { uid: newId('u'), email, name: id.name.slice(0, 80) || email, verified: true };
        statements.push(['insert users {uid: $1, email: $2, name: $3, password: "", verified: true, created: now()}', [u.uid, email, u.name]]);
      } else if (!u.verified) {
        statements.push(['set users {verified: true} where uid = $1 require 1', [u.uid]]);
        u.verified = true;
      }
      statements.push(['insert identities {subject: $1, user: $2, at: now()}', [subject, u.uid]]);
      const out = await acc.batch(statements);
      if (!out.ok) return reply(out.status === 409 ? 409 : 500, { error: 'That identity is linked already. Try again.' });
    }
    if (!u) return reply(403, { error: 'That account no longer exists.' });
    return this.#startSession(u, ip, agent, 'signin.sso');
  }

  async user(uid: string): Promise<User | undefined> {
    return this.#acc().one<User>('get users select uid, email, name, verified where uid = $1 limit 1', [uid]);
  }

  async securityLog(uid: string): Promise<Record<string, unknown>[]> {
    return this.#acc().rows('get security_log select kind, ip, detail, at where user = $1 order at desc limit 50', [uid]);
  }

  // ---------------------------------------------------------- organisations

  /**
   * A new organisation: its slug taken in the accounts tenant (the unique
   * index settles a race), its tenant placed by the router on the node with
   * the least data and given a replica on another, its schema applied, and
   * its first owner and team written.
   */
  async createOrg(c: Caller, input: { name: string; slug: string }): Promise<Reply> {
    const u = await this.user(c.sub);
    if (!u) return reply(401, { error: 'Sign in to continue.' });
    if (!u.verified) return reply(403, { error: 'Confirm your email first: the link is in your inbox.' });
    const slug = input.slug.trim().toLowerCase();
    const name = input.name.trim().slice(0, 80);
    if (!name) return reply(400, { error: 'Name the organisation.' });
    if (!SLUG.test(slug) || RESERVED.has(slug)) {
      return reply(400, { error: 'Use 3 to 40 lowercase letters, digits and dashes for the address.' });
    }
    const taken = await this.#acc().batch([['insert orgs {slug: $1, name: $2, owner: $3, created: now()}', [slug, name, u.uid]]]);
    if (!taken.ok) {
      if (taken.status === 409) return reply(409, { error: 'That address is taken. Try another.' });
      throw new Error(taken.error);
    }
    await this.#placeTenant(tenantOf(slug), ORG_SCHEMA);
    const team = newId('t');
    const out = await this.org(slug).batch(
      [
        ['insert members {user: $1, name: $2, email: $3, role: "owner", teams: $4, joined: now()} if absent', [u.uid, u.name, u.email, [team]]],
        ['insert teams {key: $1, name: "General", created: now()}', [team]],
        ['insert audit {actor: $1, action: "org.created", target: $2, detail: $3, at: now()}', [u.uid, slug, name]],
      ],
      { key: `org-create-${slug}` },
    );
    if (!out.ok) throw new Error(out.error);
    await this.#acc().batch([['put memberships {key: $1, user: $2, org: $3, at: now()} if absent', [`${u.uid}:${slug}`, u.uid, slug]]]);
    return reply(201, { slug, name });
  }

  /** An invitation, written with the admin's own token: the policy decides who may invite whom. */
  async invite(c: Caller, input: { email: string; role: string; teams: string[] }): Promise<Reply> {
    const email = normaliseEmail(input.email);
    if (!email) return reply(400, { error: 'Enter an email address like name@example.com.' });
    if (!(ROLES as readonly string[]).includes(input.role)) return reply(400, { error: 'Choose a role.' });
    const teams = (input.teams ?? []).filter((t) => typeof t === 'string').slice(0, 50);
    const code = secret();
    const out = await this.orgAs(c.org!, c.token).batch([
      ['insert invites {hash: $1, email: $2, role: $3, teams: $4, by: $5, at: now()}', [digest(code), email, input.role, teams, c.sub]],
      ['insert audit {actor: $1, action: "invite.created", target: $2, detail: $3, at: now()}', [c.sub, email, input.role]],
    ]);
    if (!out.ok) return refusal(out);
    const org = await this.#acc().one<{ name: string }>('get orgs select name where slug = $1 limit 1', [c.org]);
    await this.#mail(email, `Join ${org?.name ?? c.org} on Trellis`, [
      `${c.name} invited you to ${org?.name ?? c.org} as ${article(input.role)} ${input.role}.`,
      `Accept here, signed in with this address: ${this.cfg.origin}/invite/${c.org}/${code}`,
      'The invitation works once, for three days.',
    ]);
    return reply(201, { message: `Invitation sent to ${email}.` });
  }

  /**
   * An invitation taken up. The redemption is `insert ... if absent require
   * 1` on the invite's hash: a second use writes nothing, so its block --
   * the member, the audit row -- is put back. A lapsed invite is out of
   * the first read already (@ttl).
   */
  async acceptInvite(c: Caller, slug: string, code: string): Promise<Reply> {
    if (!SLUG.test(slug)) return reply(404, { error: 'This invitation does not exist.' });
    const u = await this.user(c.sub);
    if (!u) return reply(401, { error: 'Sign in to continue.' });
    if (!u.verified) return reply(403, { error: 'Confirm your email first: the link is in your inbox.' });
    const org = this.org(slug);
    const hash = digest(code);
    let inv: { email: string; role: Role; teams: string[] | null } | undefined;
    try {
      inv = await org.one('get invites select email, role, teams where hash = $1 limit 1', [hash]);
    } catch (e) {
      if (e instanceof DbError && e.status === 404) inv = undefined;
      else throw e;
    }
    if (!inv) return reply(410, { error: 'This invitation has expired or does not exist.' });
    if (inv.email !== u.email) return reply(403, { error: 'This invitation was sent to another address.' });
    const out = await org.batch([
      ['get invites select id where hash = $1 limit 1 require 1', [hash]],
      ['insert redemptions {invite: $1, user: $2, at: now()} if absent require 1', [hash, u.uid]],
      ['insert members {user: $1, name: $2, email: $3, role: $4, teams: $5, joined: now()}', [u.uid, u.name, u.email, inv.role, inv.teams ?? []]],
      ['insert audit {actor: $1, action: "member.joined", target: $2, detail: $3, at: now()}', [u.uid, u.email, inv.role]],
    ]);
    if (!out.ok) {
      if (out.status === 412 && out.at === 1) return reply(409, { error: 'This invitation was used already.' });
      if (out.status === 412) return reply(410, { error: 'This invitation has expired or does not exist.' });
      if (out.status === 409) return reply(409, { error: 'You are a member already.' });
      throw new Error(out.error);
    }
    await this.#acc().batch([['put memberships {key: $1, user: $2, org: $3, at: now()} if absent', [`${u.uid}:${slug}`, u.uid, slug]]]);
    return reply(200, { slug });
  }

  /** A member's role or teams changed, with the caller's own token. */
  async updateMember(c: Caller, uid: string, change: { role?: string; teams?: string[] }): Promise<Reply> {
    const statements: [string, unknown[]][] = [];
    if (change.role !== undefined) {
      if (!(ROLES as readonly string[]).includes(change.role)) return reply(400, { error: 'Choose a role.' });
      statements.push(['set members {role: $2} where user = $1 require 1', [uid, change.role]]);
      statements.push(['insert audit {actor: $1, action: "member.role", target: $2, detail: $3, at: now()}', [c.sub, uid, change.role]]);
    }
    if (change.teams !== undefined) {
      const teams = change.teams.filter((t) => typeof t === 'string').slice(0, 50);
      statements.push(['set members {teams: $2} where user = $1 require 1', [uid, teams]]);
      statements.push(['insert audit {actor: $1, action: "member.teams", target: $2, detail: $3, at: now()}', [c.sub, uid, teams.join(',')]]);
    }
    if (!statements.length) return reply(400, { error: 'Nothing to change.' });
    const out = await this.orgAs(c.org!, c.token).batch(statements);
    return out.ok ? reply(200, { message: 'Saved.' }) : refusal(out);
  }

  async removeMember(c: Caller, uid: string): Promise<Reply> {
    const out = await this.orgAs(c.org!, c.token).batch([
      ['del members where user = $1 require 1', [uid]],
      ['insert audit {actor: $1, action: "member.removed", target: $2, detail: "", at: now()}', [c.sub, uid]],
    ]);
    if (!out.ok) return refusal(out);
    await this.#acc().batch([['del memberships where key = $1', [`${uid}:${c.org}`]]]);
    return reply(200, { message: 'Removed.' });
  }

  async createTeam(c: Caller, name: string): Promise<Reply> {
    const n = name.trim().slice(0, 60);
    if (!n) return reply(400, { error: 'Name the team.' });
    const key = newId('t');
    const out = await this.orgAs(c.org!, c.token).batch([
      ['insert teams {key: $1, name: $2, created: now()}', [key, n]],
      ['insert audit {actor: $1, action: "team.created", target: $2, detail: $3, at: now()}', [c.sub, key, n]],
    ]);
    return out.ok ? reply(201, { key, name: n }) : refusal(out);
  }

  // -------------------------------------------------------------- the rest

  async #log(user: string, kind: string, ip: string, detail: string): Promise<void> {
    const out = await this.#acc().batch([['insert security_log {user: $1, kind: $2, ip: $3, detail: $4, at: now()}', [user, kind, ip, detail]]]);
    if (!out.ok) throw new Error(out.error);
  }

  /** The development mailbox: a real deployment hands this to a mail service. */
  async #mail(to: string, subject: string, lines: string[]): Promise<void> {
    const out = await this.#acc().batch([['insert outbox {to: $1, subject: $2, body: $3, at: now()}', [to, subject, lines.join('\n\n')]]]);
    if (!out.ok) throw new Error(out.error);
  }

  async outbox(to?: string): Promise<{ to: string; subject: string; body: string; at: string }[]> {
    return to
      ? this.#acc().rows('get outbox where to = $1 order id desc limit 50', [to])
      : this.#acc().rows('get outbox order id desc limit 50');
  }
}

function article(role: string): string {
  return /^[aeiou]/.test(role) ? 'an' : 'a';
}

/** A refusal from the database as the API answers it: the policy's 403 stays a 403. */
function refusal(out: { status: number; error: string }): Reply {
  if (out.status === 403) return reply(403, { error: 'Your role does not allow that.' });
  if (out.status === 412) return reply(404, { error: 'That member or team is not there.' });
  if (out.status === 409) return reply(409, { error: 'That already exists.' });
  if (out.status === 401) return reply(401, { error: 'Sign in again.' });
  throw new Error(out.error);
}
