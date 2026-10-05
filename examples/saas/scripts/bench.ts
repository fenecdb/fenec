// `npm run bench`: Trellis measured on a cluster of its own -- sign-ins and
// token refreshes a second, a board's load, how long a write takes to reach
// a subscribed board, and one organisation's reads beside a noisy
// neighbour on the same node. Each step logs as it goes; the summary is
// printed at the end (README, Measured).
//
//   BENCH_SECONDS   how long each rate is measured (default 10)
import { cpus } from 'node:os';
import { Browser, join_, organisation, person, query, start } from '../test/harness.ts';
import { sseEvents } from '../test/sse.ts';

const SECONDS = Number(process.env.BENCH_SECONDS ?? 10);
const log = (s: string) => console.log(`[${new Date().toISOString().slice(11, 19)}] ${s}`);
const pct = (xs: number[], p: number) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(s.length * p))];
};
const ms = (x: number) => `${x.toFixed(2)} ms`;
const summary: string[] = [];
const say = (line: string) => {
  log(line);
  summary.push(line);
};

/** `n` loops of `f` for SECONDS: what they did a second, and each call's latency. */
async function rate(n: number, f: (i: number, k: number) => Promise<void>): Promise<{ perSec: number; lat: number[] }> {
  const lat: number[] = [];
  const until = performance.now() + SECONDS * 1000;
  await Promise.all(
    Array.from({ length: n }, async (_, i) => {
      for (let k = 0; performance.now() < until; k++) {
        const t0 = performance.now();
        await f(i, k);
        lat.push(performance.now() - t0);
      }
    }),
  );
  return { perSec: lat.length / SECONDS, lat };
}

log(`bench on ${cpus().length} cores (${cpus()[0].model}), ${SECONDS} s a rate`);
const env = await start(19900, { scryptN: 2 ** 15, ipLimit: 1e9, accountLimit: 1e9 });
try {
  // ---- sign-ins: scrypt N = 2^15, r = 8 (32 MB, about 50 ms a hash)
  const people: Browser[] = [];
  for (let i = 0; i < 16; i++) people.push(await person(env, `p${i}@bench.io`, `Person ${i}`, `10.50.${i}.1`));
  log('16 accounts made');
  for (const n of [1, 4, 16]) {
    const r = await rate(n, async (i) => {
      const a = await new Browser(env, `10.51.${i}.1`).signIn(`p${i}@bench.io`);
      if (a.status !== 200) throw new Error(`sign-in ${a.status}`);
    });
    say(`sign-in, ${n} at once: ${r.perSec.toFixed(1)}/s, p50 ${ms(pct(r.lat, 0.5))}, p99 ${ms(pct(r.lat, 0.99))}`);
  }

  // ---- an organisation with a board of 500 tasks and four people
  const owner = people[0];
  await organisation(owner, 'acme', 'Acme');
  await owner.refresh('acme');
  const team = owner.access.get('acme')!.teams[0];
  for (let i = 1; i < 16; i++) await join_(env, owner, 'acme', people[i], `p${i}@bench.io`, 'member', [team]);
  const ownerT = await owner.token('acme');
  for (let i = 0; i < 500; i += 50) {
    const lines = Array.from({ length: 50 }, (_, k) => JSON.stringify({ query: 'insert tasks {team: $1, title: $2, body: $3, status: $4, updated: now()}', params: [team, `Task ${i + k}`, `Details of task ${i + k}, with a few words to search.`, ['backlog', 'doing', 'review', 'done'][k % 4]] }));
    const res = await fetch(`${env.cfg.routerUrl}/t/o-acme/batch`, { method: 'POST', headers: { authorization: `Bearer ${ownerT}` }, body: lines.join('\n') });
    if (!res.ok) throw new Error(await res.text());
  }
  log('board of 500 tasks made');

  // ---- token refresh: a rotation, a membership read and an RS256 signature
  for (const n of [1, 16]) {
    const r = await rate(n, async (i) => {
      const a = await people[i].refresh('acme');
      if (a.status !== 200) throw new Error(`refresh ${a.status} ${JSON.stringify(a.body)}`);
    });
    say(`token refresh with an organisation's token, ${n} at once: ${r.perSec.toFixed(0)}/s, p50 ${ms(pct(r.lat, 0.5))}, p99 ${ms(pct(r.lat, 0.99))}`);
  }

  // ---- a board's load: what the page reads when it opens a board, as a member
  const memberT = await people[1].token('acme');
  const boardLoad = async (base: string) => {
    const post = (q: string, params: unknown[] = []) =>
      fetch(`${base}/query`, { method: 'POST', headers: { authorization: `Bearer ${memberT}`, 'content-type': 'application/json' }, body: JSON.stringify({ query: q, params }) }).then((r) => r.text());
    await Promise.all([post('get teams select key, name order created limit 200'), post('get members select user, name, role order name limit 500')]);
    await post('get tasks where team = $1 order updated desc limit 500', [team]);
  };
  for (const [label, base] of [
    ['through the router', `${env.cfg.routerUrl}/t/o-acme`],
    ['through the app\'s /db/ pipe', `${env.app}/db/t/o-acme`],
  ] as const) {
    const lat: number[] = [];
    for (let i = 0; i < 300; i++) {
      const t0 = performance.now();
      await boardLoad(base);
      if (i >= 20) lat.push(performance.now() - t0);
    }
    say(`board load (teams, people, 500 tasks), ${label}: p50 ${ms(pct(lat, 0.5))}, p99 ${ms(pct(lat, 0.99))}`);
  }
  const conc = await rate(16, async () => boardLoad(`${env.cfg.routerUrl}/t/o-acme`));
  say(`board load, 16 at once: ${conc.perSec.toFixed(0)} boards/s, p50 ${ms(pct(conc.lat, 0.5))}, p99 ${ms(pct(conc.lat, 0.99))}`);

  // ---- live update: a task written to the moment a subscribed board hears it
  for (const subscribers of [1, 50]) {
    const ac = new AbortController();
    const heard = new Map<string, number>();
    let ready = 0;
    const streams = Array.from({ length: subscribers }, async (_, i) => {
      const res = await fetch(`${env.cfg.routerUrl}/t/o-acme/tasks/changes?team=eq.${team}`, { headers: { authorization: `Bearer ${memberT}` }, signal: ac.signal });
      try {
        for await (const ev of sseEvents(res)) {
          if (ev.name === 'seed') ready++;
          if (ev.name !== 'change' || i !== subscribers - 1) continue;
          for (const r of JSON.parse(ev.data).puts ?? []) if (!heard.has(r.title)) heard.set(r.title, performance.now());
        }
      } catch {
        // aborted
      }
    });
    while (ready < subscribers) await new Promise((r) => setTimeout(r, 10));
    const lat: number[] = [];
    for (let i = 0; i < 300; i++) {
      const title = `live-${subscribers}-${i}`;
      const t0 = performance.now();
      const w = await query(env, 'o-acme', memberT, 'insert tasks {team: $1, title: $2, status: "backlog", updated: now()}', [team, title]);
      if (w.status !== 200) throw new Error(w.text);
      while (!heard.has(title)) await new Promise((r) => setImmediate(r));
      lat.push(heard.get(title)! - t0);
    }
    ac.abort();
    await Promise.allSettled(streams);
    say(`live update, write sent to the last of ${subscribers} boards hearing it: p50 ${ms(pct(lat, 0.5))}, p99 ${ms(pct(lat, 0.99))}`);
  }

  // ---- a noisy neighbour: two organisations on one node
  const quietOwner = people[2];
  await organisation(quietOwner, 'quiet', 'Quiet');
  await organisation(people[3], 'noisy', 'Noisy');
  const tenants = (await (await fetch(`${env.cfg.routerUrl}/_shard/tenants`, { headers: { authorization: `Bearer ${env.cfg.shardToken}` } })).json()) as { name: string; node: string }[];
  const quietNode = tenants.find((t) => t.name === 'o-quiet')!.node;
  if (tenants.find((t) => t.name === 'o-noisy')!.node !== quietNode) {
    const m = await fetch(`${env.cfg.routerUrl}/_shard/tenants/o-noisy/move`, { method: 'POST', headers: { authorization: `Bearer ${env.cfg.shardToken}` }, body: JSON.stringify({ to: quietNode }) });
    if (!m.ok) throw new Error(await m.text());
  }
  log(`quiet and noisy both on ${quietNode}`);
  const quietT = await quietOwner.token('quiet');
  const quietTeam = quietOwner.access.get('quiet')!.teams[0];
  for (let i = 0; i < 200; i++) await query(env, 'o-quiet', quietT, 'insert tasks {team: $1, title: $2, status: "backlog", updated: now()}', [quietTeam, `Quiet ${i}`]);
  const noisyT = await people[3].token('noisy');
  const noisyTeam = people[3].access.get('noisy')!.teams[0];
  const quietReads = async (seconds: number) => {
    const lat: number[] = [];
    const until = performance.now() + seconds * 1000;
    while (performance.now() < until) {
      const t0 = performance.now();
      const r = await query(env, 'o-quiet', quietT, 'get tasks where team = $1 order updated desc limit 100', [quietTeam]);
      if (r.status !== 200) throw new Error(r.text);
      lat.push(performance.now() - t0);
    }
    return lat;
  };
  const calm = await quietReads(SECONDS);
  say(`quiet organisation's board read, alone on its node: p50 ${ms(pct(calm, 0.5))}, p99 ${ms(pct(calm, 0.99))} (${calm.length} reads)`);
  let stop = false;
  let noisyWrites = 0;
  let noisyScans = 0;
  const noise = [
    ...Array.from({ length: 6 }, async (_, w) => {
      for (let i = 0; !stop; i++) {
        const lines = Array.from({ length: 100 }, (_, k) => JSON.stringify({ query: 'insert tasks {team: $1, title: $2, body: $3, status: "backlog", updated: now()}', params: [noisyTeam, `Noise ${w}-${i}-${k}`, 'x'.repeat(400)] }));
        const res = await fetch(`${env.cfg.routerUrl}/t/o-noisy/batch`, { method: 'POST', headers: { authorization: `Bearer ${noisyT}` }, body: lines.join('\n') });
        await res.text();
        noisyWrites += 100;
      }
    }),
    ...Array.from({ length: 4 }, async () => {
      while (!stop) {
        await query(env, 'o-noisy', noisyT, 'get tasks where title ~ "zzz" count');
        noisyScans++;
      }
    }),
  ];
  const loud = await quietReads(SECONDS);
  stop = true;
  await Promise.all(noise);
  say(
    `quiet organisation's board read, beside 6 bulk writers and 4 scanners in another organisation on its node ` +
      `(${(noisyWrites / SECONDS).toFixed(0)} rows/s written, ${(noisyScans / SECONDS).toFixed(0)} scans/s): p50 ${ms(pct(loud, 0.5))}, p99 ${ms(pct(loud, 0.99))}`,
  );
} finally {
  await env.stop();
}
console.log('\nSummary\n' + summary.map((s) => `- ${s}`).join('\n'));
