// The numbers are right: every beacon counted once through retries and
// duplicates, the rollups equal to the raw events however the worker
// stopped, and the funnel, retention, bars and VWAP equal to the same
// answers worked out here by brute force.
import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { before, describe, test } from 'node:test';
import { bars, TickGenerator, windowRows, writeTicks, type Tick } from '../src/market.ts';
import { dashboard, FUNNEL, noFilters, retention, walkFunnel, type Filters } from '../src/queries.ts';
import { RollupWorker } from '../src/rollup.ts';
import { writeEvents, type Site } from '../src/setup.ts';
import { rng, Traffic, type SimEvent } from '../src/sim.ts';
import { DAY, dayOf, iso, MINUTE, minuteOf, WEEK, weekOf } from '../src/time.ts';
import { beacon, beaconBody, newMarket, newSite, pool, rows } from './helpers.ts';

const id = () => randomBytes(9).toString('base64url');

/** A simulated day's events moved into the last 50 minutes, so the ingest endpoint takes them. */
function recent(n: number, seed: number, now = Date.now()): SimEvent[] {
  const t = new Traffic({ daily: n, growth: 0, seed, prefix: `r${seed}` });
  const d = dayOf(now) - DAY;
  return t.day(d, 0).map((e) => ({ ...e, at: Math.floor(now - 50 * MINUTE + ((e.at - d) / DAY) * 48 * MINUTE) }));
}

/** Beacons as a tracker would make them: one visitor's events, 20 at most each. */
function beacons(events: SimEvent[]): { batch: string; events: SimEvent[] }[] {
  const by = new Map<string, SimEvent[]>();
  for (const e of events) (by.get(e.user) ?? by.set(e.user, []).get(e.user)!).push(e);
  const out: { batch: string; events: SimEvent[] }[] = [];
  for (const list of by.values()) for (let i = 0; i < list.length; i += 20) out.push({ batch: id(), events: list.slice(i, i + 20) });
  return out;
}

/** Every rollup against the same counts grouped from the raw events: the differences, none when they agree. */
async function differences(site: string): Promise<string[]> {
  const out: string[] = [];
  const t = (v: unknown) => iso(Date.parse(v as string));
  const same = async (what: string, raw: string, rollup: string, key: (r: Record<string, unknown>) => string, val: (r: Record<string, unknown>) => unknown) => {
    const a = new Map((await rows(site, raw)).map((r) => [key(r), val(r)]));
    const b = new Map((await rows(site, rollup)).map((r) => [r.key as string, r.v]));
    for (const [k, v] of a) if (b.get(k) !== v) out.push(`${what} ${k}: raw ${v}, rollup ${b.get(k)}`);
    for (const k of b.keys()) if (!a.has(k)) out.push(`${what} ${k}: only in the rollup`);
  };
  const lim = 'limit 1000000';
  await same('minute', `get events select bucket(at, 1m) as m, name, count(*) as n group m, name ${lim}`, `get minutes select key, n as v ${lim}`, (r) => `${t(r.m)}|${r.name}`, (r) => r.n);
  await same('day', `get events select bucket(at, 1d) as d, name, count(*) as n group d, name ${lim}`, `get days select key, n as v ${lim}`, (r) => `${t(r.d)}|${r.name}`, (r) => r.n);
  await same('page', `get events select bucket(at, 1d) as d, path, count(*) as n where name = "pageview" group d, path ${lim}`, `get day_pages select key, n as v ${lim}`, (r) => `${t(r.d)}|${r.path}`, (r) => r.n);
  await same('ref', `get events select bucket(at, 1d) as d, ref, count(*) as n where name = "pageview" group d, ref ${lim}`, `get day_refs select key, n as v ${lim}`, (r) => `${t(r.d)}|${r.ref}`, (r) => r.n);
  await same('day-user', `get events select bucket(at, 1d) as d, user, count(*) as n group d, user ${lim}`, `get day_users select key, 1 as v ${lim}`, (r) => `${t(r.d)}|${r.user}`, () => 1);
  // A week row's cohort: the week of its visitor's first day.
  const firstDay = new Map((await rows(site, `get events select user, min(bucket(at, 1d)) as f group user ${lim}`)).map((r) => [r.user as string, Date.parse(r.f as string)]));
  await same('week-user', `get events select bucket(at, 1w) as w, user, count(*) as n group w, user ${lim}`, `get weekly select key, cohort as v ${lim}`, (r) => `${t(r.w)}|${r.user}`, (r) => iso(weekOf(firstDay.get(r.user as string) as number)));
  await same('first', `get events select user, min(bucket(at, 1d)) as f group user ${lim}`, `get visitors select user as key, first as v ${lim}`, (r) => r.user as string, (r) => r.f);
  await same('last', `get events select user, max(bucket(at, 1d)) as l group user ${lim}`, `get visitors select user as key, last as v ${lim}`, (r) => r.user as string, (r) => r.l);
  const cells = new Map<string, number>();
  for (const r of await rows(site, `get events select bucket(at, 1w) as w, user, count(*) as n group w, user ${lim}`)) {
    const k = `${iso(weekOf(firstDay.get(r.user as string) as number))}|${t(r.w)}`;
    cells.set(k, (cells.get(k) ?? 0) + 1);
  }
  const kept = new Map((await rows(site, `get cohorts select key, n where n != 0 ${lim}`)).map((r) => [r.key as string, r.n]));
  for (const [k, v] of cells) if (kept.get(k) !== v) out.push(`cohort ${k}: raw ${v}, rollup ${kept.get(k)}`);
  if (cells.size !== kept.size) out.push(`cohorts: ${cells.size} raw, ${kept.size} rollups`);
  for (const d of ['country', 'device', 'browser']) {
    const a = new Map((await rows(site, `get events select bucket(at, 1d) as d, ${d} as value, count(*) as n group d, value ${lim}`)).map((r) => [`${t(r.d)}|${d}|${r.value}`, r.n]));
    const b = new Map((await rows(site, `get day_dims select key, n where dim = "${d}" ${lim}`)).map((r) => [r.key as string, r.n]));
    for (const [k, v] of a) if (b.get(k) !== v) out.push(`dim ${k}: raw ${v}, rollup ${b.get(k)}`);
    if (a.size !== b.size) out.push(`dim ${d}: ${a.size} raw, ${b.size} rollups`);
  }
  await same('firsts', `get events select bucket(at, 1d) as d, name, user, min(at) as a where name != "pageview" group d, name, user ${lim}`, `get firsts select key, at as v ${lim}`, (r) => `${t(r.d)}|${r.name}|${r.user}`, (r) => r.a);
  return out;
}

async function settle(site: string): Promise<RollupWorker> {
  const w = new RollupWorker({ site, wait: 0 });
  await w.catchUp();
  return w;
}

describe('ingest and rollups', () => {
  let site: Site;
  const events = recent(300, 5);
  const sent = beacons(events);

  before(async () => {
    site = await newSite('t-ingest');
  });

  test('time buckets agree with the server to the millisecond', async () => {
    const r = rng(9);
    const ats = Array.from({ length: 200 }, () => Math.floor(Date.now() - r() * 25 * DAY));
    await writeEvents(
      site.name,
      ats.map((at, i) => ({ eid: `tb-${i}`, name: 'bucket_probe', user: 'probe-user', path: '/', ref: '', country: 'ZZ', device: 'desktop', browser: 'Other', at, props: null })),
    );
    const got = await rows(site.name, 'get events select at, bucket(at, 1m) as m, bucket(at, 1d) as d, bucket(at, 1w) as w where name = "bucket_probe" limit 1000');
    assert.equal(got.length, ats.length);
    for (const g of got) {
      const at = Date.parse(g.at as string);
      assert.equal(Date.parse(g.m as string), minuteOf(at));
      assert.equal(Date.parse(g.d as string), dayOf(at));
      assert.equal(Date.parse(g.w as string), weekOf(at), `week of ${g.at}`);
    }
  });

  test('each beacon is counted once, through retries, duplicates sent at once and resends with a later clock', async () => {
    const statuses: number[] = [];
    // Every beacon once, 16 at a time.
    await pool(sent, 16, async (b) => {
      statuses.push((await beacon(beaconBody(site, b.batch, b.events))).status);
    });
    const r = rng(77);
    const again = sent.filter(() => r() < 0.3);
    // A retry with the same body: the server answers what it answered the first time.
    await pool(again.slice(0, 40), 8, async (b) => {
      statuses.push((await beacon(beaconBody(site, b.batch, b.events, Date.now()))).status);
    });
    // The same beacon twice at the same moment, as a slow network and a retry would send it.
    await pool(again.slice(40, 80), 8, async (b) => {
      const body = beaconBody(site, b.batch, b.events);
      const [x, y] = await Promise.all([beacon(body), beacon(body)]);
      statuses.push(x.status, y.status);
    });
    // A beacon held and sent again later: its clock moved, so the request differs (422 behind the endpoint).
    await pool(again.slice(80), 8, async (b) => {
      statuses.push((await beacon(beaconBody(site, b.batch, b.events, Date.now() + 1500))).status);
    });
    assert.ok(statuses.every((s) => s === 204), `statuses: ${[...new Set(statuses)]}`);
    const [c] = await rows(site.name, 'get events select count(*) as n, count(distinct eid) as ids where name != "bucket_probe"');
    assert.equal(c.n, events.length, 'one row an event');
    assert.equal(c.ids, events.length);
    // Each visitor's events, as sent.
    const per = await rows(site.name, 'get events select user, name, count(*) as n where name != "bucket_probe" group user, name limit 100000');
    const want = new Map<string, number>();
    for (const e of events) want.set(`${e.user.padEnd(8, '0')}|${e.name}`, (want.get(`${e.user.padEnd(8, '0')}|${e.name}`) ?? 0) + 1);
    assert.equal(per.length, want.size);
    for (const p of per) assert.equal(p.n, want.get(`${p.user}|${p.name}`), `${p.user} ${p.name}`);
  });

  test('a beacon written under a new key is still written once: the events keep their ids', async () => {
    const b = sent[0];
    const before = (await rows(site.name, 'get events select count(*) as n'))[0].n;
    // The operator writes the same events again, as a replayed import would.
    await writeEvents(
      site.name,
      b.events.map((e, i) => ({ ...e, eid: `${b.batch}:${i}` })),
    );
    assert.equal((await rows(site.name, 'get events select count(*) as n'))[0].n, before);
  });

  test('the rollups equal the raw events, and a worker that crashed after its block lands nothing twice', async () => {
    // One page applied, then the worker "dies" before it notes that it did.
    const a = new RollupWorker({ site: site.name, wait: 0, limit: 500 });
    await a.open();
    await a.step({ crash: 'after' });
    // A second worker starts from where the database says, not where the first thought.
    const b = await settle(site.name);
    assert.deepEqual(await differences(site.name), []);
    // The first one comes back and sends its stale page again: refused whole.
    await a.step();
    assert.ok(a.refused >= 1, 'the stale page was refused');
    assert.deepEqual(await differences(site.name), []);
    const [s] = await rows(site.name, 'get rollup_state limit 1');
    const [raw] = await rows(site.name, 'get events select count(*) as n');
    assert.equal(s.events, raw.n, 'every event folded once');
    assert.ok(b.applied > 0);
  });

  test('two workers racing over one site still fold each event once', async () => {
    const more = recent(80, 6);
    await pool(beacons(more), 16, async (b) => {
      assert.equal((await beacon(beaconBody(site, b.batch, b.events))).status, 204);
    });
    const w1 = new RollupWorker({ site: site.name, wait: 0, limit: 300 });
    const w2 = new RollupWorker({ site: site.name, wait: 0, limit: 300 });
    await Promise.all([w1.catchUp(), w2.catchUp()]);
    await settle(site.name);
    assert.deepEqual(await differences(site.name), []);
    const [s] = await rows(site.name, 'get rollup_state limit 1');
    const [raw] = await rows(site.name, 'get events select count(*) as n');
    assert.equal(s.events, raw.n);
  });

  test('a rebuild from the raw events, as after a 410, gives the same rollups', async () => {
    const w = new RollupWorker({ site: site.name, wait: 0 });
    await w.open();
    await w.rebuild(Date.now() - 2 * 3_600_000);
    assert.equal(w.rebuilds, 1);
    assert.deepEqual(await differences(site.name), []);
  });
});

describe('funnel, retention and totals against brute force', () => {
  let site: Site;
  const now = Date.now();
  let events: SimEvent[] = [];

  before(async () => {
    site = await newSite('t-history');
    // 27 days of history, inside the raw events' 30.
    const t = new Traffic({ daily: 120, growth: 0.01, seed: 31, prefix: 'h' });
    const start = dayOf(now) - 27 * DAY;
    for (let i = 0; i <= 27; i++) events.push(...t.day(start + i * DAY, i).filter((e) => e.at < now - MINUTE));
    events = events.map((e) => ({ ...e, user: e.user.padEnd(8, '0') }));
    await writeEvents(site.name, events);
    await settle(site.name);
  });

  test('the rollups equal the raw events over four weeks', async () => {
    assert.deepEqual(await differences(site.name), []);
  });

  /** Every visitor walked: a visit, then each step no earlier than the first time of the one before. */
  function funnelOf(list: SimEvent[]): number[] {
    const first = new Map<string, number>();
    for (const e of list) {
      const k = `${e.user}|${e.name}`;
      if (!first.has(k) || e.at < (first.get(k) as number)) first.set(k, e.at);
    }
    const users = new Set(list.map((e) => e.user));
    const steps: string[] = FUNNEL.slice(1).map((s) => s.name);
    const out = [users.size, ...steps.map(() => 0)];
    for (const u of users) {
      let last = -Infinity;
      for (let i = 0; i < steps.length; i++) {
        const t = first.get(`${u}|${steps[i]}`);
        if (t === undefined || t < last) break;
        out[i + 1]++;
        last = t;
      }
    }
    // The walk the dashboard uses, over the same first times, agrees.
    const rowsOf = [...first].filter(([k]) => steps.includes(k.split('|')[1])).map(([k, at]) => ({ user: k.split('|')[0], name: k.split('|')[1], first: at }));
    assert.deepEqual(walkFunnel(rowsOf, steps), out.slice(1));
    return out;
  }

  const inRange = (from: number, f?: Filters) => (e: SimEvent) =>
    e.at >= from && (!f || ((!f.country.length || f.country.includes(e.country)) && (!f.device.length || f.device.includes(e.device)) && (!f.browser.length || f.browser.includes(e.browser))));

  test('the funnel, from the raw events and from the rollups, unfiltered and filtered, is what walking every visitor gives', async () => {
    const day = await dashboard(site.name, '24h', noFilters(), now);
    assert.equal(day.source, 'raw');
    assert.deepEqual(
      day.funnel.map((s) => s.users),
      funnelOf(events.filter(inRange(day.rawFrom))),
    );
    const d = await dashboard(site.name, '30d', noFilters(), now);
    assert.equal(d.source, 'rollups');
    assert.deepEqual(
      d.funnel.map((s) => s.users),
      funnelOf(events.filter(inRange(d.rawFrom))),
    );
    assert.ok(d.funnel[2].users > 0, 'some visitors finished a signup');
    const f: Filters = { country: ['US', 'DE'], device: ['mobile'], browser: [] };
    const g = await dashboard(site.name, '7d', f, now);
    assert.equal(g.source, 'raw');
    assert.deepEqual(
      g.funnel.map((s) => s.users),
      funnelOf(events.filter(inRange(g.rawFrom, f))),
    );
  });

  test('weekly retention is what the visitors and their weeks give', async () => {
    const { cohorts } = await retention(site.name, now);
    const first = new Map<string, number>();
    const weeks = new Map<string, Set<number>>();
    for (const e of events) {
      first.set(e.user, Math.min(first.get(e.user) ?? Infinity, dayOf(e.at)));
      (weeks.get(e.user) ?? weeks.set(e.user, new Set()).get(e.user)!).add(weekOf(e.at));
    }
    const want = new Map<number, { size: number; active: number[] }>();
    for (const [u, f] of first) {
      const c = weekOf(f);
      if (c < weekOf(now) - 7 * WEEK) continue;
      const n = Math.floor((weekOf(now) - c) / WEEK) + 1;
      const w = want.get(c) ?? want.set(c, { size: 0, active: new Array(n).fill(0) }).get(c)!;
      w.size++;
      for (const wk of weeks.get(u) as Set<number>) w.active[(wk - c) / WEEK]++;
    }
    assert.deepEqual(
      cohorts.map((c) => [c.week, c.size, c.active]),
      [...want].sort((a, b) => a[0] - b[0]).map(([k, v]) => [k, v.size, v.active]),
    );
  });

  test('a late event moves its visitor to an earlier cohort, and every count follows', async () => {
    // Visitors first seen in the last few days, each sent an event from three weeks back.
    const recentUsers = [...new Set(events.filter((e) => e.at > now - 3 * DAY).map((e) => e.user))].slice(0, 25);
    const late = recentUsers.map((user, i) => ({ ...events.find((e) => e.user === user)!, eid: `late-${i}`, name: 'signup_started', at: now - 21 * DAY + i * 60_000 }));
    await writeEvents(site.name, late);
    await settle(site.name);
    assert.deepEqual(await differences(site.name), []);
    events.push(...late);
  });

  test('totals and series: raw for a day, the rollups for a week, both as the events give', async () => {
    const d = await dashboard(site.name, '24h', noFilters(), now);
    assert.equal(d.source, 'raw');
    const day = events.filter(inRange(d.from));
    assert.equal(d.totals.events, day.length);
    assert.equal(d.totals.views, day.filter((e) => e.name === 'pageview').length);
    assert.equal(d.totals.visitors, new Set(day.map((e) => e.user)).size);
    assert.equal(
      d.series.reduce((s, p) => s + p.views, 0),
      d.totals.views,
    );
    const w = await dashboard(site.name, '7d', noFilters(), now);
    assert.equal(w.source, 'rollups');
    const week = events.filter(inRange(w.from));
    assert.equal(w.totals.events, week.length);
    assert.equal(w.totals.views, week.filter((e) => e.name === 'pageview').length);
    assert.equal(w.totals.visitors, new Set(week.map((e) => e.user)).size);
    const pages = new Map<string, number>();
    for (const e of week) if (e.name === 'pageview') pages.set(e.path, (pages.get(e.path) ?? 0) + 1);
    for (const p of w.pages) assert.equal(p.views, pages.get(p.path), p.path);
  });
});

describe('market data against brute force', () => {
  let tenant: string;
  const now = Date.now();
  const ticks: Tick[] = [];

  before(async () => {
    tenant = await newMarket();
    const gen = new TickGenerator(3);
    // 40 minutes of ticks, a distinct millisecond each, written out of order in seven batches.
    const all = gen.ticks(now - 40 * MINUTE, now - 1000, 6000);
    const seen = new Set<number>();
    for (const t of all) {
      while (seen.has(t.at)) t.at++;
      seen.add(t.at);
      ticks.push(t);
    }
    const r = rng(4);
    const shuffled = [...ticks].sort(() => r() - 0.5);
    for (let i = 0; i < shuffled.length; i += 900) await writeTicks(shuffled.slice(i, i + 900), tenant);
  });

  const close = (a: number, b: number, what: string) => assert.ok(Math.abs(a - b) <= 1e-9 * Math.max(1, Math.abs(b)), `${what}: ${a} against ${b}`);

  test('bars: open, high, low and close by time, volume and VWAP, whatever order the ticks came in', async () => {
    for (const sym of [...new Set(ticks.map((t) => t.sym))].slice(0, 6)) {
      for (const [interval, step] of [
        ['1m', MINUTE],
        ['5m', 5 * MINUTE],
      ] as const) {
        const got = await bars(sym, 60, interval, now, tenant);
        const by = new Map<number, Tick[]>();
        for (const t of ticks) if (t.sym === sym && t.at >= now - 60 * MINUTE) (by.get(Math.floor(t.at / step) * step) ?? by.set(Math.floor(t.at / step) * step, []).get(Math.floor(t.at / step) * step)!).push(t);
        assert.equal(got.length, by.size, `${sym} ${interval}: bars`);
        for (const b of got) {
          const list = (by.get(Date.parse(b.bar)) as Tick[]).sort((x, y) => x.at - y.at);
          assert.equal(b.open, list[0].px);
          assert.equal(b.close, list[list.length - 1].px);
          assert.equal(b.high, Math.max(...list.map((t) => t.px)));
          assert.equal(b.low, Math.min(...list.map((t) => t.px)));
          const vol = list.reduce((s, t) => s + t.qty, 0);
          assert.equal(b.volume, vol);
          close(b.vwap, list.reduce((s, t) => s + t.px * t.qty, 0) / vol, `${sym} ${b.bar} VWAP`);
        }
      }
    }
  });

  test("the window's row a symbol and the kept quotes agree with the ticks", async () => {
    const got = await windowRows(30, now, tenant);
    const quotes = new Map((await rows(tenant, 'get quotes limit 100')).map((q) => [q.sym as string, q]));
    for (const r of got) {
      const list = ticks.filter((t) => t.sym === r.sym && t.at >= now - 30 * MINUTE).sort((a, b) => a.at - b.at);
      assert.equal(r.open, list[0].px);
      assert.equal(r.px, list[list.length - 1].px);
      assert.equal(r.trades, list.length);
      close(r.vwap, list.reduce((s, t) => s + t.px * t.qty, 0) / list.reduce((s, t) => s + t.qty, 0), `${r.sym} VWAP`);
      const all = ticks.filter((t) => t.sym === r.sym).sort((a, b) => a.at - b.at);
      const q = quotes.get(r.sym) as Record<string, number>;
      assert.equal(q.px, all[all.length - 1].px, `${r.sym}: the quote is the latest tick's, though batches came out of order`);
      assert.equal(q.high, Math.max(...all.map((t) => t.px)));
      assert.equal(q.low, Math.min(...all.map((t) => t.px)));
      assert.equal(q.n, all.length);
    }
  });
});
