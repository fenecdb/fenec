// Clients of the tenant node, one per tenant and role, through
// `@fenecdb/web/client` (the HTTP client and the builder, no WebAssembly).
// A client carries one token; tokens live ten minutes, so a client is made
// again every five.
import { connect, type FenecHttp } from '@fenecdb/web/client';
import { OPERATOR_TOKEN, tenantUrl } from './config.ts';
import { tokens, type Role } from './tokens.ts';

const kept = new Map<string, { db: FenecHttp; until: number }>();

/** `role`'s client of `tenant`, or the node's own (`'operator'`) for setup and the rollup worker. */
export function db(tenant: string, role: Role | 'operator'): FenecHttp {
  const key = `${tenant}\u0000${role}`;
  const now = Date.now();
  const k = kept.get(key);
  if (k && k.until > now) return k.db;
  const token = role === 'operator' ? OPERATOR_TOKEN : tokens.get(tenant, role);
  const d = connect(tenantUrl(tenant), { token });
  kept.set(key, { db: d, until: now + 240_000 });
  return d;
}
