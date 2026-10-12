// Plover's tenants, placed and filled: the platform's, and one a company,
// each made through the router with its schema (FenecQL, from lib/schema.ts)
// and a company's with its plan's quota. Then the people, couriers and
// merchants, and days of past deliveries for the dashboards. Run again, it
// adds what is missing: a tenant that exists is kept (409), rows go in with
// `put ... if absent`, which passes over a row whose unique key is taken.
import { toFenecQL } from '@fenecdb/web/schema';
import { platform, company } from './schema';
import { PLANS, type Quota } from './plans';
import { COMPANIES, DEMO_PASSWORD, merchants, people, wave, type CompanySpec } from './world';
import { PLATFORM, tenantOf } from './config';
import { addresses, metresBetween, rng, route } from './city';
import { hashPassword } from './passwords';
import { operatorDb, retrying, routerUrl, type Db } from './db';
import { connect } from '@fenecdb/web/client';
import { OPERATOR_TOKEN } from './config';
import { nodeFetch } from './fetch';

export interface SetupOptions {
  router: string;
  shardToken: string;
  companies?: CompanySpec[];
  /** Days of delivered orders before today. */
  historyDays?: number;
  /** Place every tenant on this node: the quota tests want two companies side by side. */
  node?: string;
  log?: (s: string) => void;
}

/** A tenant placed with its schema, and a quota; one that exists is kept. */
export async function placeTenant(o: SetupOptions, tenant: string, schema: string, quota?: Quota): Promise<boolean> {
  const res = await fetch(`${o.router}/_shard/tenants/${tenant}`, {
    method: 'PUT',
    headers: { authorization: `Bearer ${o.shardToken}`, 'content-type': 'application/json' },
    body: JSON.stringify({ schema, ...(quota ? { quota } : {}), ...(o.node ? { node: o.node } : {}) }),
  });
  if (res.status === 409) return false;
  if (!res.ok) throw new Error(`place ${tenant}: ${res.status} ${await res.text()}`);
  return true;
}

/** A company's quota, the plan's or none (0 everywhere, `{}`) while a bulk load runs. */
export async function setQuota(o: Pick<SetupOptions, 'router' | 'shardToken'>, tenant: string, quota: Partial<Quota>): Promise<void> {
  const res = await fetch(`${o.router}/_shard/tenants/${tenant}/quota`, {
    method: 'PUT',
    headers: { authorization: `Bearer ${o.shardToken}`, 'content-type': 'application/json' },
    body: JSON.stringify(quota),
  });
  if (!res.ok) throw new Error(`quota ${tenant}: ${res.status} ${await res.text()}`);
}

/** Documents in pages of `n`, each one statement: `put <c> $1 if absent`. */
async function putAll(db: Db, collection: string, docs: Record<string, unknown>[], n = 500): Promise<void> {
  for (let i = 0; i < docs.length; i += n) {
    await retrying(() => db.run(`put ${collection} $1 if absent`, [docs.slice(i, i + n)]));
  }
}

/**
 * A tenant made before the code's schema changed: what the code adds -- a
 * collection, a field, an index -- applied on the node (`/_schema/apply`
 * through the router, the operator's token), and anything that would lose
 * data refused, naming it. A new index on an existing tenant is built
 * beside its writes.
 */
async function migrate(tenant: string, schema: typeof platform | typeof company): Promise<void> {
  await connect(`${routerUrl()}/t/${tenant}`, { token: OPERATOR_TOKEN, schema, migrate: true, fetch: nodeFetch });
}

export async function setup(o: SetupOptions): Promise<void> {
  const log = o.log ?? (() => {});
  const specs = o.companies ?? COMPANIES;
  if (!(await placeTenant(o, PLATFORM, toFenecQL(platform)))) await migrate(PLATFORM, platform);
  const plat = operatorDb(PLATFORM);
  // One hash for every seeded account: they share the demo password, and
  // scrypt at 2^14 takes 40 ms a hash -- 700 of them would be half a minute.
  const hash = hashPassword(DEMO_PASSWORD);
  const now = Date.now();
  for (const c of specs) {
    const tenant = tenantOf(c.slug);
    // Placed without its quota and given it after the load: the plan's
    // bytes a second would hold a backfill of days of orders to a minute or
    // more of 429s, the operator's token included.
    const fresh = await placeTenant(o, tenant, toFenecQL(company));
    if (!fresh) await migrate(tenant, company);
    log(`${tenant}: ${fresh ? 'placed' : 'there, its schema brought up to the code'}`);
    await putAll(plat, 'companies', [{ slug: c.slug, name: c.name, plan: c.plan, created: now }]);
    const folk = people(c);
    await putAll(
      plat,
      'users',
      folk.map((p) => ({ uid: p.uid, email: p.email, name: p.name, password: hash, company: c.slug, role: p.role, code: p.code, created: now })),
    );
    const db = operatorDb(tenant);
    await putAll(
      db,
      'couriers',
      folk.filter((p) => p.role === 'courier').map((p) => ({ user: p.uid, code: p.code, name: p.name, vehicle: p.vehicle })),
    );
    const shops = merchants(c);
    await putAll(db, 'merchants', shops.map((m) => ({ code: m.code, name: m.name, kind: m.kind, address: m.address, loc: m.loc })));
    if (fresh && (o.historyDays ?? 0) > 0) {
      const docs = history(c, o.historyDays!, now);
      await putAll(db, 'orders', docs);
      log(`${tenant}: ${docs.length} past orders`);
    }
    await setQuota(o, tenant, PLANS[c.plan].quota);
  }
}

/**
 * Past deliveries, `days` of them up to the start of today: orders in the
 * day's waves, picked up minutes after they were placed, carried along the
 * streets at a vehicle's pace, and late now and then -- more so at the
 * height of dinner, when there are fewer couriers than orders.
 */
export function history(c: CompanySpec, days: number, now: number): Record<string, unknown>[] {
  const r = rng(c.seed * 31337);
  const shops = merchants(c);
  const folk = people(c);
  const riders = folk.filter((p) => p.role === 'courier');
  const buyers = folk.filter((p) => p.role === 'customer');
  const doors = addresses();
  // Merrow keeps UTC+3: its days start at 21:00 UTC the evening before.
  const today = new Date(now);
  today.setUTCHours(0, 0, 0, 0);
  const localMidnight = today.getTime() - 3 * 3_600_000 + (now - today.getTime() >= 21 * 3_600_000 ? 86_400_000 : 0);
  const out: Record<string, unknown>[] = [];
  let n = 0;
  for (let d = days; d >= 1; d--) {
    const day = localMidnight - d * 86_400_000;
    const weekend = [0, 6].includes(new Date(day + 3 * 3_600_000).getUTCDay());
    for (let h = 0; h < 24; h++) {
      const want = (c.ordersPerDay / 9) * wave(h) * (weekend ? 1.2 : 1) * (0.85 + 0.3 * r());
      const count = Math.floor(want) + (r() < want % 1 ? 1 : 0);
      for (let k = 0; k < count; k++) {
        n++;
        const shop = shops[Math.floor(r() * shops.length)];
        const buyer = buyers[Math.floor(r() * buyers.length)];
        // Most customers order from near home; a door near the merchant
        // stands in for a few of them.
        const door = r() < 0.6 ? buyer.home : doors[Math.floor(r() * doors.length)];
        const rider = riders[Math.floor(r() * riders.length)];
        const created = day + h * 3_600_000 + Math.floor(r() * 3_600_000);
        const pace = rider.vehicle === 'bike' ? 4.2 : rider.vehicle === 'scooter' ? 6.5 : 7.5;
        const straight = metresBetween(shop.loc, door.point);
        // A route a past order in twenty, the rest by the street factor
        // those measured: A* for each of 12 000 orders is seconds of setup.
        const metres = n % 20 === 0 ? route(shop.loc, door.point).metres : straight * 1.32 + 120;
        const toPickup = 300 + r() * 1800;
        // At the height of a wave there are fewer couriers than orders,
        // and an order waits for one: some are late.
        const busy = Math.max(0, wave(h) - 0.55) / 0.45;
        const wait = 90_000 + r() * 360_000 + busy * r() * r() * 1_800_000;
        const picked = created + wait + (toPickup / pace) * 1000;
        const delivered = picked + (metres / pace) * 1000 + 60_000;
        out.push({
          ref: `${shop.code}-${String(n).padStart(6, '0')}`,
          merchant: shop.code,
          customer: buyer.uid,
          pickup: shop.loc,
          pickup_name: shop.name,
          dropoff: door.point,
          dropoff_addr: door.label,
          items: 1 + Math.floor(r() * 5),
          fee: 250 + Math.round(metres / 10) * 5,
          status: 'delivered',
          run_at: null,
          courier: rider.uid,
          claim: null,
          attempts: r() < 0.04 ? 2 : 1,
          created,
          promised: created + 45 * 60_000,
          picked_at: Math.round(picked),
          delivered_at: Math.round(delivered),
          driven: Math.round(toPickup + metres),
        });
      }
    }
  }
  return out;
}
