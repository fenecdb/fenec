// What the tests share: a cluster of their own (three tenant nodes and the
// router, over a new directory), Trellis and the mock identity provider in
// this process, and people who sign up, confirm their address and join
// organisations the way a browser would -- through the API, a cookie jar
// each.
import { mkdtempSync, rmSync } from 'node:fs';
import { type Server } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { startApp } from '../src/app.ts';
import { Cluster } from '../src/cluster.ts';
import { config, type Config } from '../src/config.ts';
import { idpServer } from '../src/idp.ts';
import { peek } from '../src/jwt.ts';
import type { Trellis } from '../src/trellis.ts';

export const PASSWORD = 'correct horse battery';

export interface Env {
  cfg: Config;
  cluster: Cluster;
  trellis: Trellis;
  app: string;
  idp: string;
  stop(): Promise<void>;
}

/**
 * Everything on ports from `base`: the router at base, nodes after it, the
 * app at base + 10 and the identity provider at base + 11.
 */
export async function start(base: number, over: Partial<Config> & { lease?: number; nodeFlags?: string[] } = {}): Promise<Env> {
  const dir = mkdtempSync(join(tmpdir(), 'trellis-test-'));
  const { lease, nodeFlags, ...rest } = over;
  const cfg = config({
    routerUrl: `http://127.0.0.1:${base}`,
    publicRouterUrl: `http://127.0.0.1:${base}`,
    keysDir: join(dir, 'keys'),
    origin: `http://127.0.0.1:${base + 10}`,
    oidcIssuer: `http://127.0.0.1:${base + 11}`,
    insecureCookies: true,
    trustProxy: true,
    scryptN: 2 ** 12,
    shardToken: 'test-shard',
    operatorToken: 'test-operator',
    ...rest,
  });
  const cluster = new Cluster({
    dir,
    basePort: base,
    lease: lease ?? 0,
    keysDir: cfg.keysDir,
    operatorToken: cfg.operatorToken,
    shardToken: cfg.shardToken,
    nodeFlags: ['--http-max-streams', '512', ...(nodeFlags ?? [])],
  });
  await cluster.start();
  const { trellis, server, url } = await startApp(cfg, base + 10);
  const idp: Server = idpServer({ issuer: cfg.oidcIssuer, keysDir: cfg.keysDir, clients: { [cfg.oidcClientId]: `${cfg.origin}/api/auth/sso/callback` } });
  await new Promise<void>((r) => idp.listen(base + 11, '127.0.0.1', r));
  return {
    cfg,
    cluster,
    trellis,
    app: url,
    idp: cfg.oidcIssuer,
    async stop() {
      server.closeAllConnections();
      idp.closeAllConnections();
      await new Promise((r) => server.close(r));
      await new Promise((r) => idp.close(r));
      await cluster.stop();
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

export interface Answer {
  status: number;
  body: Record<string, unknown> & { error?: string };
  headers: Headers;
}

/** A browser, more or less: a cookie jar, an address, and the app's API. */
export class Browser {
  jar = new Map<string, string>();
  api?: string;
  access = new Map<string, { token: string; role: string; teams: string[] }>();
  uid?: string;

  constructor(
    readonly env: Env,
    readonly ip = '10.0.0.1',
  ) {}

  async call(method: string, path: string, body?: unknown, token?: string): Promise<Answer> {
    const headers: Record<string, string> = { 'x-forwarded-for': this.ip, 'user-agent': 'trellis-test' };
    if (body !== undefined) headers['content-type'] = 'application/json';
    if (token) headers.authorization = `Bearer ${token}`;
    const cookie = [...this.jar].map(([k, v]) => `${k}=${v}`).join('; ');
    if (cookie) headers.cookie = cookie;
    const res = await fetch(`${this.env.app}${path}`, { method, headers, body: body === undefined ? undefined : JSON.stringify(body), redirect: 'manual' });
    for (const c of res.headers.getSetCookie()) {
      const [pair, ...attrs] = c.split(';');
      const [k, v] = [pair.slice(0, pair.indexOf('=')), pair.slice(pair.indexOf('=') + 1)];
      if (attrs.some((a) => a.trim() === 'Max-Age=0')) this.jar.delete(k);
      else this.jar.set(k, v);
    }
    const text = await res.text();
    let parsed: Record<string, unknown> = {};
    try {
      parsed = text ? JSON.parse(text) : {};
    } catch {
      parsed = { text };
    }
    return { status: res.status, body: parsed, headers: res.headers };
  }

  post(path: string, body: unknown = {}, token?: string) {
    return this.call('POST', path, body, token);
  }

  async signUp(email: string, name: string, password = PASSWORD): Promise<Answer> {
    return this.post('/api/auth/signup', { email, name, password });
  }

  async signIn(email: string, password = PASSWORD): Promise<Answer> {
    const r = await this.post('/api/auth/signin', { email, password });
    if (r.status === 200) this.#keep(r);
    return r;
  }

  async refresh(org?: string): Promise<Answer> {
    const r = await this.post('/api/auth/refresh', org === undefined ? {} : { org });
    if (r.status === 200) this.#keep(r);
    return r;
  }

  #keep(r: Answer) {
    this.api = (r.body.api as { token: string }).token;
    this.uid = (r.body.user as { uid: string }).uid;
    const a = r.body.access as { org: string; token: string; role: string; teams: string[] } | undefined;
    if (a) this.access.set(a.org, a);
  }

  /** The access token for an organisation, fresh. */
  async token(org: string): Promise<string> {
    const r = await this.refresh(org);
    if (r.status !== 200) throw new Error(`no token for ${org}: ${r.status} ${JSON.stringify(r.body)}`);
    return this.access.get(org)!.token;
  }
}

/** The latest link of a kind mailed to an address. */
export async function mailedLink(env: Env, to: string, path: string): Promise<string> {
  const mails = await env.trellis.outbox(to);
  for (const m of mails) {
    const found = new RegExp(`${env.cfg.origin}${path}[^\\s]+`).exec(m.body);
    if (found) return found[0].slice(env.cfg.origin.length);
  }
  throw new Error(`no ${path} mail to ${to}`);
}

/** Someone with a confirmed account, signed in. */
export async function person(env: Env, email: string, name: string, ip?: string): Promise<Browser> {
  const b = new Browser(env, ip ?? `10.${Math.floor(Math.random() * 250)}.${Math.floor(Math.random() * 250)}.${Math.floor(Math.random() * 250)}`);
  const up = await b.signUp(email, name);
  if (up.status !== 202) throw new Error(`sign up ${email}: ${up.status} ${JSON.stringify(up.body)}`);
  const link = await mailedLink(env, email.toLowerCase(), '/verify\\?token=');
  const v = await b.post('/api/auth/verify', { token: new URL(link, 'http://x').searchParams.get('token') });
  if (v.status !== 200) throw new Error(`verify ${email}: ${v.status}`);
  const s = await b.signIn(email);
  if (s.status !== 200) throw new Error(`sign in ${email}: ${s.status} ${JSON.stringify(s.body)}`);
  return b;
}

/** An organisation made by `owner`. */
export async function organisation(owner: Browser, slug: string, name = slug): Promise<void> {
  const r = await owner.post('/api/orgs', { name, slug }, owner.api);
  if (r.status !== 201) throw new Error(`org ${slug}: ${r.status} ${JSON.stringify(r.body)}`);
}

/** `who` invited into `slug` by `admin` and joined. */
export async function join_(env: Env, admin: Browser, slug: string, who: Browser, email: string, role: string, teams: string[]): Promise<void> {
  const inv = await admin.post(`/api/orgs/${slug}/invites`, { email, role, teams }, await admin.token(slug));
  if (inv.status !== 201) throw new Error(`invite ${email}: ${inv.status} ${JSON.stringify(inv.body)}`);
  const link = await mailedLink(env, email.toLowerCase(), `/invite/${slug}/`);
  const code = link.split('/').pop()!;
  await who.refresh();
  const r = await who.post('/api/invites/accept', { org: slug, code }, who.api);
  if (r.status !== 200) throw new Error(`accept ${email}: ${r.status} ${JSON.stringify(r.body)}`);
}

/** A request straight to the router, with any token: what someone holding one could send. */
export async function direct(env: Env, method: string, path: string, token: string | undefined, body?: unknown): Promise<{ status: number; body: unknown; text: string }> {
  const headers: Record<string, string> = {};
  if (token !== undefined) headers.authorization = `Bearer ${token}`;
  if (body !== undefined) headers['content-type'] = 'application/json';
  const res = await fetch(`${env.cfg.routerUrl}${path}`, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
  const text = await res.text();
  let parsed: unknown = text;
  try {
    parsed = JSON.parse(text);
  } catch {
    // an error text or a stream
  }
  return { status: res.status, body: parsed, text };
}

export const query = (env: Env, tenant: string, token: string | undefined, q: string, params: unknown[] = []) =>
  direct(env, 'POST', `/t/${tenant}/query`, token, { query: q, params });

export { peek };
