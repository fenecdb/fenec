// An organisation moved between nodes while its people keep writing, and a
// node killed under them: every write that was answered is there after,
// once, and the tokens people hold keep working, since every node checks
// them with the same public key and the tenant travels whole.
import assert from 'node:assert/strict';
import { after, before, describe, test } from 'node:test';
import { join_, organisation, person, query, start, type Env } from './harness.ts';
import { sseEvents } from './sse.ts';

const T = 'o-acme';

interface Placement {
  name: string;
  node: string;
  replica?: string;
  state: string;
}

async function placement(env: Env, tenant: string): Promise<Placement> {
  const res = await fetch(`${env.cfg.routerUrl}/_shard/tenants`, { headers: { authorization: `Bearer ${env.cfg.shardToken}` } });
  const all = (await res.json()) as Placement[];
  return all.find((t) => t.name === tenant)!;
}

/**
 * Writers inserting tasks with an idempotency key each, retrying a write
 * with the same key on a 503 (a move's freeze, a lapsed lease), a 502 or a
 * dropped connection, until it is answered. What was answered is what
 * must be there.
 */
function writers(env: Env, token: string, team: string, n: number) {
  let stop = false;
  const acked: string[] = [];
  const latencies: number[] = [];
  let retries = 0;
  const statuses = new Map<number, number>();
  const one = async (w: number) => {
    for (let i = 0; !stop; i++) {
      const title = `w${w}-${i}`;
      const t0 = performance.now();
      for (let attempt = 0; ; attempt++) {
        let status = 0;
        try {
          const res = await fetch(`${env.cfg.routerUrl}/t/${T}/query`, {
            method: 'POST',
            headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json', 'idempotency-key': `k-${title}` },
            body: JSON.stringify({ query: 'insert tasks {team: $1, title: $2, status: "backlog", updated: now()}', params: [team, title] }),
          });
          status = res.status;
          await res.text();
        } catch {
          status = 0;
        }
        statuses.set(status, (statuses.get(status) ?? 0) + 1);
        if (status === 200) break;
        if (status !== 0 && status < 500) throw new Error(`write ${title}: ${status}`);
        retries++;
        await new Promise((r) => setTimeout(r, Math.min(200, 10 * 2 ** attempt)));
      }
      acked.push(title);
      latencies.push(performance.now() - t0);
    }
  };
  const running = Promise.all(Array.from({ length: n }, (_, w) => one(w)));
  return {
    acked,
    latencies,
    statuses,
    get retries() {
      return retries;
    },
    async stop() {
      stop = true;
      await running;
    },
  };
}

async function titles(env: Env): Promise<string[]> {
  const r = await query(env, T, env.cfg.operatorToken, 'get tasks select title limit 100000');
  assert.equal(r.status, 200, r.text);
  return (r.body as { title: string }[]).map((x) => x.title);
}

function pct(xs: number[], p: number): number {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(s.length * p))];
}

const until = async (pred: () => boolean | Promise<boolean>, ms = 15_000) => {
  const t0 = Date.now();
  while (!(await pred())) {
    if (Date.now() - t0 > ms) throw new Error('timed out');
    await new Promise((r) => setTimeout(r, 10));
  }
};

describe('a tenant moved under load', () => {
  let env: Env;
  before(async () => {
    env = await start(19700);
  });
  after(() => env.stop());

  test('loses no answered write, and sessions and subscriptions carry on', async () => {
    const owner = await person(env, 'olivia@acme.io', 'Olivia');
    await organisation(owner, 'acme', 'Acme');
    const team = (await owner.refresh('acme'), owner.access.get('acme')!.teams[0]);
    const mia = await person(env, 'mia@acme.io', 'Mia');
    await join_(env, owner, 'acme', mia, 'mia@acme.io', 'member', [team]);
    const token = await mia.token('acme');

    const from = await placement(env, T);
    const to = env.cluster.nodeNames.find((n) => n !== from.node && n !== from.replica)!;

    // A subscriber, caught up before the move.
    const ac = new AbortController();
    const res = await fetch(`${env.cfg.routerUrl}/t/${T}/tasks/changes`, { headers: { authorization: `Bearer ${token}` }, signal: ac.signal });
    const events: string[] = [];
    let ended = false;
    const reading = (async () => {
      try {
        for await (const ev of sseEvents(res)) events.push(ev.name);
      } catch {
        // aborted
      }
      ended = true;
    })();

    const w = writers(env, token, team, 8);
    await until(() => w.acked.length >= 200);
    const t0 = performance.now();
    const moved = await fetch(`${env.cfg.routerUrl}/_shard/tenants/${T}/move`, {
      method: 'POST',
      headers: { authorization: `Bearer ${env.cfg.shardToken}` },
      body: JSON.stringify({ to }),
    });
    const moveMs = performance.now() - t0;
    const body = await moved.json();
    assert.equal(moved.status, 200, JSON.stringify(body));
    const atMove = w.acked.length;
    await until(() => w.acked.length >= atMove + 400);
    await w.stop();

    const there = await titles(env);
    const set = new Set(there);
    const lost = w.acked.filter((t) => !set.has(t));
    assert.equal(lost.length, 0, `lost ${lost.length}: ${lost.slice(0, 5)}`);
    assert.equal(there.length, set.size, 'no write made twice by a retry');
    assert.equal((await placement(env, T)).node, to);
    console.log(
      `  moved ${from.node} -> ${to} (${body.bytes} bytes) in ${moveMs.toFixed(0)} ms under 8 writers: ${w.acked.length} writes answered, ` +
        `${w.retries} retried (${[...w.statuses].map(([s, n]) => `${s}: ${n}`).join(', ')}), none lost; ` +
        `write p50 ${pct(w.latencies, 0.5).toFixed(1)} ms, p99 ${pct(w.latencies, 0.99).toFixed(1)}, max ${Math.max(...w.latencies).toFixed(0)}`,
    );

    // The token Mia held before the move reads on the new node, her
    // session refreshes, and the old node no longer serves the tenant.
    assert.equal((await query(env, T, token, 'get tasks limit 1')).status, 200);
    assert.equal((await mia.refresh('acme')).status, 200);
    const old = await fetch(`${env.cluster.nodeUrl(from.node)}/t/${T}/query`, {
      method: 'POST',
      headers: { authorization: `Bearer ${token}` },
      body: JSON.stringify({ query: 'get tasks limit 1' }),
    });
    assert.equal(old.status, 404);
    // The subscription ended with the source's copy; opened again it seeds
    // from the target.
    await until(() => ended);
    ac.abort();
    await reading;
    const again = await fetch(`${env.cfg.routerUrl}/t/${T}/tasks/changes`, { headers: { authorization: `Bearer ${token}` } });
    assert.equal(again.status, 200);
    for await (const ev of sseEvents(again)) {
      assert.equal(ev.name, 'seed');
      assert.equal(JSON.parse(ev.data).rows.length, there.length);
      break;
    }
  });
});

describe('a node killed under load', () => {
  let env: Env;
  before(async () => {
    // Nodes write only under the router's lease of a second, and the router
    // fails a silent node over once that has certainly lapsed.
    env = await start(19800, { lease: 1 });
  });
  after(() => env.stop());

  test('its tenants come back on their replicas with every answered write', async () => {
    const owner = await person(env, 'olivia@acme.io', 'Olivia');
    await organisation(owner, 'acme', 'Acme');
    const team = (await owner.refresh('acme'), owner.access.get('acme')!.teams[0]);
    const token = await owner.token('acme');
    const from = await placement(env, T);
    assert.ok(from.replica, 'the tenant has a replica');

    const w = writers(env, token, team, 8);
    await until(() => w.acked.length >= 300);
    const killedAt = performance.now();
    await env.cluster.killNode(from.node);
    const atKill = w.acked.length;
    await until(() => w.acked.length >= atKill + 1, 30_000);
    const outage = performance.now() - killedAt;
    await until(() => w.acked.length >= atKill + 300, 30_000);
    await w.stop();

    const now_ = await placement(env, T);
    assert.equal(now_.node, from.replica, 'promoted on its replica');
    const set = new Set(await titles(env));
    const lost = w.acked.filter((t) => !set.has(t));
    console.log(
      `  killed ${from.node}; writes answered again on ${now_.node} ${outage.toFixed(0)} ms later; ` +
        `${w.acked.length} answered, ${lost.length} lost, ${w.retries} retried`,
    );
    assert.equal(lost.length, 0, `lost ${lost.slice(0, 5)}`);
    // The owner's session goes on: her token reads there, and refreshes.
    assert.equal((await query(env, T, token, 'get tasks limit 1')).status, 200);
    assert.equal((await owner.refresh('acme')).status, 200);
  });
});
