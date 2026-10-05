// The dashboard's questions, each one FenecQL statement under the site's
// viewer token, sent side by side: a /batch would read one state, but it
// takes the write lock, reads and all, and would hold ingest back for as
// long as the slowest of them (README, "Gaps").
//
// Where the answers come from. Up to a day, and whenever a filter is set,
// from the raw events: they hold every field, so any filter and any page
// can be asked of them, and over a day of a site's traffic a scan of the
// range is a few milliseconds. Past a day, unfiltered, from the rollups:
// a week of raw events is seven times the rows, and the raw events are
// kept 30 days only, so 90 days could not be answered from them at all.
// The switch is at `RAW_UNTIL`; README, "Measured", has the latencies
// either side of it.
import { FenecError } from '@fenecdb/web/client';
import { db } from './db.ts';
import { DAY, dayOf, HOUR, interval, iso, MINUTE, WEEK, weekOf } from './time.ts';

export const RANGES = {
  '1h': { ms: HOUR, step: MINUTE, label: 'Last hour' },
  '24h': { ms: DAY, step: 30 * MINUTE, label: 'Last 24 hours' },
  '7d': { ms: 7 * DAY, step: 6 * HOUR, label: 'Last 7 days' },
  '30d': { ms: 30 * DAY, step: DAY, label: 'Last 30 days' },
  '90d': { ms: 90 * DAY, step: DAY, label: 'Last 90 days' },
} as const;
export type Range = keyof typeof RANGES;
export const isRange = (r: unknown): r is Range => typeof r === 'string' && r in RANGES;

/** Ranges up to this read the raw events; longer ones, unfiltered, the rollups. */
export const RAW_UNTIL = DAY;
/** How long the raw events are kept (schema.fenecql's @ttl). */
export const RAW_KEPT = 30 * DAY;

export const FACETS = ['country', 'device', 'browser'] as const;
export type Facet = (typeof FACETS)[number];
export type Filters = Record<Facet, string[]>;
export const noFilters = (): Filters => ({ country: [], device: [], browser: [] });
export const filtered = (f: Filters) => FACETS.some((k) => f[k].length > 0);

/**
 * The funnel's steps, in order: a visit (any event), then events by name.
 * A visit comes first by definition -- a visitor's first event is no later
 * than any other -- so its count is the range's visitors.
 */
export const FUNNEL = [
  { name: '', label: 'Visited' },
  { name: 'signup_started', label: 'Started a signup' },
  { name: 'signup_completed', label: 'Finished a signup' },
] as const;
const STEPS = FUNNEL.slice(1).map((s) => s.name);

export interface Point {
  t: number;
  views: number;
  visitors: number | null;
}

export interface Dashboard {
  range: Range;
  source: 'raw' | 'rollups';
  from: number;
  to: number;
  step: number;
  /** The width a visitor count covers: the step, or a day from the rollups. */
  visitorStep: number;
  series: Point[];
  totals: { views: number; visitors: number; events: number };
  pages: { path: string; views: number; visitors: number | null }[];
  refs: { ref: string; views: number; visitors: number | null }[];
  facets: Record<Facet, { value: string; count: number }[]>;
  /** Where the facets and the funnel start: the range's, from the raw events at most 30 days back. */
  rawFrom: number;
  funnel: { name: string; label: string; users: number }[];
  retention: Cohort[];
  /** Each question's time, ms. */
  timings: Record<string, number>;
  /** What the page should say it could not answer; a visitor count of -1 is one of those. */
  notice?: string;
}

export interface Cohort {
  week: number;
  size: number;
  /** Visitors active in the cohort's week and each after it. */
  active: number[];
}

type Rows = Record<string, unknown>[];

/** A filter's terms, `and country in [$k, ...]`, its values after `first` parameters. */
function filterText(f: Filters, first: number): { text: string; params: string[] } {
  let text = '';
  const params: string[] = [];
  for (const k of FACETS) {
    if (!f[k].length) continue;
    const marks = f[k].map((v) => {
      params.push(v);
      return `$${first + params.length}`;
    });
    text += ` and ${k} in [${marks.join(', ')}]`;
  }
  return { text, params };
}

const num = (v: unknown) => Number(v ?? 0);

export async function dashboard(site: string, range: Range, filters: Filters, now = Date.now()): Promise<Dashboard> {
  const spec = RANGES[range];
  const raw = spec.ms <= RAW_UNTIL || filtered(filters);
  const step = spec.step;
  const n = Math.round(spec.ms / step);
  // Whole buckets, the last one the bucket now is in.
  const from = Math.max(Math.floor(now / step) * step - (n - 1) * step, raw ? now - RAW_KEPT : 0);
  const to = now + 1;
  const rawFrom = Math.max(from, now - RAW_KEPT);
  const v = db(site, 'viewer');
  const timings: Record<string, number> = {};
  const ask = async (name: string, text: string, params: unknown[]): Promise<Rows> => {
    const t = performance.now();
    try {
      return (await v.rows(text, params)) as Rows;
    } finally {
      timings[name] = performance.now() - t;
    }
  };
  const facetsAsk = async (name: string, text: string, params: unknown[]) => {
    const t = performance.now();
    try {
      const r = await v.run(text, params);
      return (r.facets ?? {}) as Dashboard['facets'];
    } finally {
      timings[name] = performance.now() - t;
    }
  };

  const f = filterText(filters, 2);
  const W = `at >= $1 and at < $2${f.text}`;
  const rp = [iso(rawFrom), iso(to), ...f.params];
  const views = 'sum(case when name = "pageview" then 1 else 0 end)';

  const ret = retention(site, now);
  const k = rp.length;
  const stepMarks = STEPS.map((_, i) => `$${k + i + 1}`).join(', ');
  // The facets, and each step's first time for every visitor who took a
  // step past the first: from the raw events in range, or the rollups.
  const side = raw
    ? Promise.all([
        facetsAsk('facets', `get events where ${W} limit 0 facet country top 12 disjunctive, device disjunctive, browser top 8 disjunctive`, rp),
        ask('funnel', `get events select user, name, min(at) as first where ${W} and name in [${stepMarks}] group user, name`, [...rp, ...STEPS]),
      ])
    : Promise.all([
        ask('facets', 'get day_dims select dim, value, sum(n) as count where day >= $1 and day < $2 group dim, value order count desc', [iso(dayOf(from)), iso(to)]).then((r) => {
          const out: Dashboard['facets'] = { country: [], device: [], browser: [] };
          for (const x of r) {
            const list = out[x.dim as Facet];
            if (list && list.length < (x.dim === 'country' ? 12 : 8)) list.push({ value: x.value as string, count: num(x.count) });
          }
          return out;
        }),
        ask('funnel', `get firsts select user, name, min(at) as first where day >= $1 and day < $2 and name in [$3, $4] group user, name`, [iso(dayOf(from)), iso(to), ...STEPS]),
      ]);

  // Awaited below; marked handled now, so a failure of the questions in
  // between does not leave these rejected with no one listening.
  side.catch(() => {});
  ret.catch(() => {});

  let series: Point[];
  let totals: Dashboard['totals'];
  let pages: Dashboard['pages'];
  let refs: Dashboard['refs'];
  let visitorStep = step;
  let notice: string | undefined;
  if (raw) {
    const p = [iso(from), iso(to), ...f.params];
    const questions = (distinct: string) =>
      Promise.all([
        ask('series', `get events select bucket(at, ${interval(step)}) as t, ${views} as views, ${distinct} as visitors where ${W} group t`, p),
        ask('totals', `get events select ${views} as views, ${distinct} as visitors, count(*) as events where ${W}`, p),
        ask('pages', `get events select path, count(*) as views, ${distinct} as visitors where ${W} and name = "pageview" group path order views desc limit 10`, p),
        ask('refs', `get events select ref, count(*) as views, ${distinct} as visitors where ${W} and name = "pageview" group ref order views desc limit 10`, p),
      ]);
    let answers: Rows[];
    try {
      answers = await questions('count(distinct user)');
    } catch (e) {
      // `count(distinct)` refuses past a million values rather than count
      // short: a month of a busy site, filtered by little. The counts of
      // events stand; the visitors are said to be past it.
      if (!(e instanceof FenecError) || !/count\(distinct/.test(e.message)) throw e;
      notice = 'More than a million visitors in this range: a distinct count is refused past a million values rather than cut short. Narrow the range or add a filter for exact visitor counts.';
      answers = await questions('-1');
    }
    const [s, t, pg, rf] = answers;
    const by = new Map(s.map((r) => [Date.parse(r.t as string), r]));
    series = Array.from({ length: n }, (_, i) => {
      const at = Math.floor(now / step) * step - (n - 1 - i) * step;
      const r = by.get(at);
      return { t: at, views: num(r?.views), visitors: num(r?.visitors) };
    }).filter((x) => x.t >= from - step);
    totals = { views: num(t[0]?.views), visitors: num(t[0]?.visitors), events: num(t[0]?.events) };
    const vis = (r: Record<string, unknown>) => (notice ? null : num(r.visitors));
    pages = pg.map((r) => ({ path: r.path as string, views: num(r.views), visitors: vis(r) }));
    refs = rf.map((r) => ({ ref: r.ref as string, views: num(r.views), visitors: vis(r) }));
    if (notice) for (const x of series) x.visitors = null;
  } else {
    // Rollups: whole days from the first day of the range.
    const fromDay = dayOf(from);
    const p = [iso(fromDay), iso(to)];
    const fine = step < DAY;
    const [s, vs, t, tv, pg, rf] = await Promise.all([
      fine
        ? ask('series', `get minutes select bucket(at, ${interval(step)}) as t, sum(n) as views where at >= $1 and at < $2 and name = "pageview" group t`, p)
        : ask('series', 'get days select day as t, sum(n) as views where day >= $1 and day < $2 and name = "pageview" group t', p),
      ask('visitors', 'get day_users select bucket(day, 1d) as t, count(*) as visitors where day >= $1 and day < $2 group t', p),
      ask('totals', 'get days select sum(case when name = "pageview" then n else 0 end) as views, sum(n) as events where day >= $1 and day < $2', p),
      // The range ends now, so its visitors are those last seen in it.
      ask('distinct', 'get visitors select count(*) as visitors where last >= $1', [iso(fromDay)]),
      ask('pages', 'get day_pages select path, sum(n) as views where day >= $1 and day < $2 group path order views desc limit 10', p),
      ask('refs', 'get day_refs select ref, sum(n) as views where day >= $1 and day < $2 group ref order views desc limit 10', p),
    ]);
    const viewsAt = new Map(s.map((r) => [Date.parse(r.t as string), num(r.views)]));
    const visitorsAt = new Map(vs.map((r) => [Date.parse(r.t as string), num(r.visitors)]));
    visitorStep = DAY;
    const start = fine ? Math.floor(now / step) * step - (n - 1) * step : fromDay;
    const count = fine ? n : Math.round((dayOf(now) - fromDay) / DAY) + 1;
    series = Array.from({ length: count }, (_, i) => {
      const at = start + i * step;
      return { t: at, views: viewsAt.get(at) ?? 0, visitors: visitorsAt.get(dayOf(at)) ?? 0 };
    });
    totals = { views: num(t[0]?.views), events: num(t[0]?.events), visitors: num(tv[0]?.visitors) };
    pages = pg.map((r) => ({ path: r.path as string, views: num(r.views), visitors: null }));
    refs = rf.map((r) => ({ ref: r.ref as string, views: num(r.views), visitors: null }));
  }
  const [[facets, firsts], r] = await Promise.all([side, ret]);
  timings.retention = r.ms;
  const reached = walkFunnel(
    firsts.map((x) => ({ user: x.user as string, name: x.name as string, first: Date.parse(x.first as string) })),
    STEPS,
  );
  return {
    range,
    source: raw ? 'raw' : 'rollups',
    from: raw ? Math.max(from, now - RAW_KEPT) : dayOf(from),
    to: now,
    step,
    visitorStep,
    series,
    totals,
    pages,
    refs,
    facets: { country: facets.country ?? [], device: facets.device ?? [], browser: facets.browser ?? [] },
    rawFrom: raw ? rawFrom : dayOf(from),
    notice,
    funnel: FUNNEL.map((x, i) => ({ name: x.name, label: x.label, users: i === 0 ? totals.visitors : reached[i - 1] })),
    retention: r.cohorts,
    timings,
  };
}

/**
 * Visitors reaching each step after the first, from each visitor's first
 * time at each: a visitor reaches step k when they took it no earlier than
 * the first time they took step k-1.
 */
export function walkFunnel(rows: { user: string; name: string; first: number }[], names: readonly string[]): number[] {
  const by = new Map<string, Map<string, number>>();
  for (const r of rows) {
    let m = by.get(r.user);
    if (!m) by.set(r.user, (m = new Map()));
    m.set(r.name, r.first);
  }
  const reached = names.map(() => 0);
  for (const m of by.values()) {
    let last = -Infinity;
    for (let i = 0; i < names.length; i++) {
      const t = m.get(names[i]);
      if (t === undefined || t < last) break;
      reached[i]++;
      last = t;
    }
  }
  return reached;
}

/** Cohorts of the last eight weeks. */
export const COHORTS = 8;

/**
 * Weekly retention from the rollups, one statement over `cohorts`: the
 * visitors of each cohort -- the week they first came -- who came in each
 * week, so a cohort's size is its own week's count. Grouped from `weekly`,
 * a row for each visitor and week, it read every one of them: 560 to 700
 * ms a dashboard at ten million events.
 */
export async function retention(site: string, now = Date.now()): Promise<{ cohorts: Cohort[]; ms: number }> {
  const t = performance.now();
  const first = weekOf(now) - (COHORTS - 1) * WEEK;
  const rows = await db(site, 'viewer').rows('get cohorts select cohort, week, n where cohort >= $1 limit 1000', [iso(first)]);
  const by = new Map<number, Map<number, number>>();
  for (const r of rows) {
    const c = Date.parse(r.cohort);
    if (!by.has(c)) by.set(c, new Map());
    (by.get(c) as Map<number, number>).set(Date.parse(r.week), num(r.n));
  }
  const cohorts = [...by.keys()]
    .sort((a, b) => a - b)
    .map((week) => {
      const at = by.get(week) as Map<number, number>;
      const weeks = Math.floor((weekOf(now) - week) / WEEK) + 1;
      return { week, size: at.get(week) ?? 0, active: Array.from({ length: weeks }, (_, i) => at.get(week + i * WEEK) ?? 0) };
    });
  return { cohorts, ms: performance.now() - t };
}

/** The "now" row the rollup worker keeps. */
export interface Pulse {
  active: number;
  views: number;
  minutes: number[];
  at: string;
}
