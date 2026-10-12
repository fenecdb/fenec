// A world of its own for each test file: two tenant nodes and the router
// over a new directory, the platform's tenant and the companies placed and
// filled with their people, couriers and merchants -- no history unless
// asked, so a test counts only what it did.
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Cluster } from '../lib/cluster';
import { OPERATOR_TOKEN, SHARD_TOKEN } from '../lib/config';
import { routeTo } from '../lib/db';
import { setup } from '../lib/setup';
import { COMPANIES, merchants, people, type CompanySpec, type Person } from '../lib/world';
import type { Who } from '../lib/tokens';

export interface World {
  cluster: Cluster;
  router: string;
  stop(): Promise<void>;
}

export async function startWorld(opts: { basePort: number; node?: string; companies?: CompanySpec[]; cors?: string; historyDays?: number }): Promise<World> {
  const dir = mkdtempSync(join(tmpdir(), 'plover-test-'));
  const cluster = new Cluster({
    dir,
    basePort: opts.basePort,
    nodes: 2,
    operatorToken: OPERATOR_TOKEN,
    shardToken: SHARD_TOKEN,
    cors: opts.cors,
  });
  await cluster.start();
  routeTo(cluster.routerUrl);
  await setup({ router: cluster.routerUrl, shardToken: SHARD_TOKEN, companies: opts.companies ?? COMPANIES, node: opts.node, historyDays: opts.historyDays ?? 0 });
  return {
    cluster,
    router: cluster.routerUrl,
    async stop() {
      await cluster.stop();
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

export const tandem = COMPANIES.find((c) => c.slug === 'tandem')!;
export const hopper = COMPANIES.find((c) => c.slug === 'hopper')!;

export function cast(c: CompanySpec) {
  const folk = people(c);
  const as = (p: Person): Who => ({ sub: p.uid, role: p.role, company: c.slug });
  return {
    couriers: folk.filter((p) => p.role === 'courier').map(as),
    customers: folk.filter((p) => p.role === 'customer'),
    customer: (i: number): Who => as(folk.filter((p) => p.role === 'customer')[i]),
    dispatcher: as(folk.find((p) => p.role === 'dispatcher')!),
    merchants: merchants(c),
    folk,
  };
}

/** The quantile `q` of `xs`, sorted here. */
export function quantile(xs: number[], q: number): number {
  if (!xs.length) return NaN;
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(q * s.length))];
}

export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
