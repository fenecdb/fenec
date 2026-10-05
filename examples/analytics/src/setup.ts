// Tenants made ready: Kestrel's own (the sites and who signs in), a tenant
// per site with its schema, and the market data tenant. Runs with the
// node's admin and operator tokens.
import { ADMIN_TOKEN, CONTROL, CONTROL_SCHEMA, FENEC_URL, MARKETS, MARKETS_SCHEMA, OPERATOR_TOKEN, SITE_SCHEMA, tenantUrl } from './config.ts';
import { db } from './db.ts';
import { insertText } from './ingest.ts';
import type { SimEvent } from './sim.ts';
import { hashPassword } from './users.ts';

export async function createTenant(tenant: string, schema: string): Promise<void> {
  const res = await fetch(`${FENEC_URL}/_admin/tenants/${tenant}`, { method: 'PUT', headers: { authorization: `Bearer ${ADMIN_TOKEN}` } });
  if (!res.ok && res.status !== 409) throw new Error(`create tenant ${tenant}: ${res.status} ${await res.text()}`);
  const applied = await fetch(`${tenantUrl(tenant)}/_schema/apply`, {
    method: 'POST',
    headers: { authorization: `Bearer ${OPERATOR_TOKEN}`, 'content-type': 'application/json' },
    body: JSON.stringify({ format: 1, fenecql: schema }),
  });
  if (!applied.ok) throw new Error(`schema of ${tenant}: ${applied.status} ${await applied.text()}`);
}

export interface Site {
  name: string;
  key: string;
  label: string;
  origins: string[];
  rate: number;
}

export async function addSite(site: Site): Promise<void> {
  await createTenant(site.name, SITE_SCHEMA);
  await db(CONTROL, 'operator').run('put sites {name: $1, key: $2, label: $3, origins: $4, rate: $5} if absent', [
    site.name,
    site.key,
    site.label,
    site.origins,
    site.rate,
  ]);
}

export async function addUser(name: string, display: string, password: string, sites: string[]): Promise<void> {
  await db(CONTROL, 'operator').run('put users {name: $1, display: $2, hash: $3, sites: $4} if absent', [name, display, hashPassword(password), sites]);
}

export async function setupControl(): Promise<void> {
  await createTenant(CONTROL, CONTROL_SCHEMA);
  await createTenant(MARKETS, MARKETS_SCHEMA);
}

/**
 * Events written straight into a site's raw collection with the node's
 * token, at the times they carry -- history the ingest endpoint, which
 * takes events no older than an hour, would refuse. The rollup worker
 * picks them up from the change stream like any other; those older than
 * 30 days leave the raw collection at once (@ttl) and live on in the
 * rollups alone.
 */
export async function writeEvents(site: string, events: SimEvent[], per = 500): Promise<number> {
  const d = db(site, 'operator');
  let seq = 0;
  for (let i = 0; i < events.length; i += per * 8) {
    const statements: [string, unknown[]][] = [];
    for (let j = i; j < Math.min(events.length, i + per * 8); j += per) {
      const part = events.slice(j, j + per);
      const params: unknown[] = [];
      for (const e of part) {
        params.push(e.eid, e.name, e.user, e.path, e.ref, e.country, e.device, e.browser, new Date(e.at).toISOString(), e.props);
      }
      statements.push([insertText(part.length), params]);
    }
    const r = await d.batch(statements);
    seq = r.seq ?? seq;
  }
  return seq;
}

export const DEMO_PASSWORD = process.env.KESTREL_DEMO_PASSWORD ?? 'kestrel-demo';

export const DEMO_SITES: Site[] = [
  { name: 'fieldnotes', key: 'fn4k7q2m9x', label: 'Fieldnotes', origins: ['https://fieldnotes.example', 'http://127.0.0.1:3000', 'http://localhost:3000'], rate: 2000 },
  { name: 'tidepool', key: 'tp8w3n6r1c', label: 'Tidepool', origins: ['https://tidepool.example'], rate: 500 },
];
