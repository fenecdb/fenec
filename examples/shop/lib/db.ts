// How the shop's server reaches fenec-server. Two kinds of token:
//
// - the server's own (`FENEC_TOKEN`), for the catalog and for checkout,
//   whose stock moves across every shopper's orders;
// - a JSON Web Token minted per shopper (`as(owner)`), for the cart and the
//   order history. The server holds it to `policy.txt` -- `where owner =
//   $jwt.sub` -- so a bug here that passed another shopper's id would read
//   nothing and write nothing: the database refuses, not just this code.
import 'server-only';
import { connect, FenecError, type FenecHttp, type SchemaOf } from '@fenecdb/web/client';
import { schema } from './schema';
import { mint } from './jwt';

type Shop = FenecHttp<SchemaOf<typeof schema>>;

export const FENEC_URL = process.env.FENEC_URL ?? 'http://127.0.0.1:8080';
const ROOT = process.env.FENEC_TOKEN ?? 'shop-dev-token'; // a dev default; never deploy it

/** The catalog's connection: the server's token, the code's schema checked once. */
let rootDb: Promise<Shop> | null = null;
export function db(): Promise<Shop> {
  rootDb ??= connect(FENEC_URL, { token: ROOT, schema }).catch((e: unknown) => {
    rootDb = null;
    throw e;
  });
  return rootDb;
}

/** A connection that can only reach `owner`'s rows (policy.txt). */
export function as(owner: string): Shop {
  return connect<SchemaOf<typeof schema>>(FENEC_URL, { token: tokenFor(owner) });
}

// A token is minted for ten minutes and used for five: the server keeps a
// verified token by its text, so reusing one skips its signature check.
const tokens = new Map<string, { token: string; until: number }>();
export function tokenFor(sub: string, role?: string): string {
  const key = `${sub}\u0000${role ?? ''}`;
  const now = Date.now();
  const kept = tokens.get(key);
  if (kept && kept.until > now) return kept.token;
  const token = mint({ sub, ...(role ? { role } : {}) }, 600);
  if (tokens.size > 10_000) tokens.clear();
  tokens.set(key, { token, until: now + 300_000 });
  return token;
}

export type Statement = [query: string, params?: unknown[]];

export interface BatchResult {
  status: number;
  replayed: boolean;
  /** Each statement's answer, in order, when the batch landed. */
  results: unknown[];
  /** Why it stopped, and at which statement (from 0), when it did not. */
  error?: string;
  at?: number;
}

/**
 * `POST /batch`: the statements as one block -- all of them land or none
 * do -- and, with `key`, an `Idempotency-Key`, so a retry after a timeout
 * is answered as the first was and writes nothing twice.
 * `@fenecdb/web/client` has no batch of its own (README, "Gaps").
 */
export async function batch(statements: Statement[], opts: { key?: string; token?: string } = {}): Promise<BatchResult> {
  const headers: Record<string, string> = {
    'content-type': 'application/x-ndjson',
    authorization: `Bearer ${opts.token ?? ROOT}`,
  };
  if (opts.key) headers['idempotency-key'] = opts.key;
  const body = statements.map(([query, params]) => JSON.stringify({ query, params: params ?? [] })).join('\n');
  const res = await fetch(`${FENEC_URL}/batch`, { method: 'POST', headers, body, cache: 'no-store' });
  const text = await res.text();
  let json: { results?: unknown[]; error?: string; at?: number } = {};
  try {
    json = text ? JSON.parse(text) : {};
  } catch {
    throw new FenecError(`fenec-server did not answer JSON (${res.status}): ${text.slice(0, 200)}`);
  }
  return {
    status: res.status,
    replayed: res.headers.get('idempotent-replayed') === 'true',
    results: json.results ?? [],
    error: json.error,
    at: json.at,
  };
}

/** One statement over `/query`, for what the builder cannot say. */
export async function query<T = Record<string, unknown>>(text: string, params: unknown[] = [], token = ROOT): Promise<T[]> {
  const res = await fetch(`${FENEC_URL}/query`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', authorization: `Bearer ${token}` },
    body: JSON.stringify({ query: text, params }),
    cache: 'no-store',
  });
  const json = await res.json();
  if (!res.ok) throw new FenecError(json?.error ?? `HTTP ${res.status}`);
  return Array.isArray(json) ? json : (json.rows ?? []);
}
