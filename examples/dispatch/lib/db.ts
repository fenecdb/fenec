// How Plover reaches its tenants: through the router, a tenant's prefix
// (`/t/<tenant>/`) and a token for one person or for the app. A refusal
// comes back as a FenecError whose `status` says what it was -- 412 a
// `require` not met (a lease lost), 409 a value taken, 403 outside the
// token's rules, 429 and 507 the company's quota (`quota`, `retryAfter`).
import { connect, FenecError, type FenecHttp } from '@fenecdb/web/client';
import { OPERATOR_TOKEN, PLATFORM, ROUTER_URL, tenantOf } from './config';
import { appToken, tokenFor, type Who } from './tokens';
import { nodeFetch } from './fetch';

export { FenecError };

export type Db = FenecHttp;

let router = ROUTER_URL;

/** Points every connection made after it at another router: a test's cluster. */
export function routeTo(url: string): void {
  router = url.replace(/\/$/, '');
  conns.clear();
}

export const routerUrl = () => router;

const conns = new Map<string, Db>();

function conn(tenant: string, token: string): Db {
  const key = `${tenant}\u0000${token}`;
  let c = conns.get(key);
  if (!c) {
    if (conns.size > 5_000) conns.clear();
    c = connect(`${router}/t/${tenant}`, { token, fetch: nodeFetch });
    conns.set(key, c);
  }
  return c;
}

/** The platform's tenant under the app's token: accounts and companies. */
export const platformDb = () => conn(PLATFORM, appToken());

/** A company's tenant under the app's token: reads only (policy.txt). */
export const appDb = (company: string) => conn(tenantOf(company), appToken(company));

/** A company's tenant under a person's own token, held to their role's rules. */
export const asUser = (who: Who) => conn(tenantOf(who.company!), tokenFor(who));

/** A tenant under the operator's token: setup and the tests' checks alone. */
export const operatorDb = (tenant: string) => conn(tenant, OPERATOR_TOKEN);

/** 429s met, by quota, and the seconds waited for them: what the tests and the bench report. */
export const throttled = { count: 0, waitedMs: 0, by: {} as Record<string, number> };

/**
 * A write that a quota refused with 429 is sent again after the server's
 * `Retry-After`; anything else is thrown. The header is whole seconds, a
 * second at least, even where the writes bucket refills a token in under
 * two milliseconds: a client that does as it is told waits that second.
 */
export async function retrying<T>(f: () => Promise<T>, tries = 50): Promise<T> {
  for (let i = 0; ; i++) {
    try {
      return await f();
    } catch (e) {
      if (!(e instanceof FenecError) || e.status !== 429 || i >= tries) throw e;
      const ms = Math.max(50, (e.retryAfter ?? 0.2) * 1000);
      throttled.count++;
      throttled.waitedMs += ms;
      throttled.by[e.quota ?? '?'] = (throttled.by[e.quota ?? '?'] ?? 0) + 1;
      await new Promise((r) => setTimeout(r, ms));
    }
  }
}

/** One statement's rows. */
export async function rows<T = Record<string, unknown>>(db: Db, text: string, params: unknown[] = []): Promise<T[]> {
  return (await db.rows(text, params)) as T[];
}

/** The first row, or undefined. */
export async function one<T = Record<string, unknown>>(db: Db, text: string, params: unknown[] = []): Promise<T | undefined> {
  return (await rows<T>(db, text, params))[0];
}

/** A write's count. */
export async function exec(db: Db, text: string, params: unknown[] = []): Promise<number> {
  const r = (await db.run(text, params)) as { count?: number; rows?: unknown[] };
  return r.count ?? r.rows?.length ?? 0;
}
