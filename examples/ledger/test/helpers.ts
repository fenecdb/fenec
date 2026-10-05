// What the tests share. They run against a fenec-server tenant node
// already started (scripts/ci.sh, or `npm run db`), each file in tenants of
// its own, so the invariants it checks are its own writes'.
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { Fenec } from '@fenecdb/web';
import { FENEC_URL, OPERATOR_TOKEN, SCHEMA, tenantUrl } from '../src/config.ts';
import { Ledger } from '../src/ledger.ts';
import { createTenant } from '../src/setup.ts';
import { HttpStore, LocalStore } from '../src/store.ts';
import { Tokens } from '../src/tokens.ts';

export { FENEC_URL };

/** A tenant made for this test alone. */
export async function freshTenant(
  label: string,
): Promise<{ tenant: string; tokens: Tokens; app: HttpStore; operator: HttpStore; ledger: (rateLimit?: number) => Ledger }> {
  const tenant = `${label}-${Date.now().toString(36)}${Math.floor(Math.random() * 1e6).toString(36)}`.toLowerCase();
  await createTenant(tenant);
  const tokens = new Tokens(tenant);
  const app = new HttpStore(tenantUrl(tenant), tokens.app());
  const operator = new HttpStore(tenantUrl(tenant), OPERATOR_TOKEN);
  return {
    tenant,
    tokens,
    app,
    operator,
    ledger: (rateLimit = 1e9) => new Ledger(new HttpStore(tenantUrl(tenant), tokens.app()), { rateLimit }),
  };
}

/** The engine in this process, with the ledger's schema. */
export async function localStore(): Promise<LocalStore> {
  const wasm = readFileSync(createRequire(import.meta.url).resolve('@fenecdb/web/fenec.wasm'));
  return new LocalStore(await Fenec.open(wasm, { schema: SCHEMA }));
}

/** A request straight to fenec-server, with any token. */
export async function raw(
  path: string,
  init: RequestInit & { token?: string } = {},
): Promise<{ status: number; body: unknown; headers: Headers }> {
  const headers = new Headers(init.headers);
  if (init.token !== undefined) headers.set('authorization', `Bearer ${init.token}`);
  const res = await fetch(`${FENEC_URL}${path}`, { ...init, headers });
  const text = await res.text();
  let body: unknown = text;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    // an NDJSON stream, or text
  }
  return { status: res.status, body, headers: res.headers };
}

export const query = (tenant: string, token: string, q: string, params: unknown[] = []) =>
  raw(`/t/${tenant}/query`, {
    method: 'POST',
    token,
    body: JSON.stringify({ query: q, params }),
    headers: { 'content-type': 'application/json' },
  });
