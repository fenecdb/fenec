// The rollup worker: a site's raw events, read from its change stream
// (`/_changes`), folded into counts by minute, day, page, referrer, visitor
// and week.
//
// Why from the stream and not at ingest: written with each beacon, the
// rollups made every beacon a block of a dozen statements, each a lookup by
// a unique key, under the one write lock -- ingest fell from 333 000 to
// 29 000 events a second in the spike. From the stream, ingest writes one
// statement a beacon, and the worker folds a page of up to 2 000 events into
// one block whose statements number the distinct keys the page touches, not
// its events (README, "Measured", has both).
//
// The block writes what a page adds as a few statements a collection: the
// worker reads first which of the page's keys have rows already, and
// writes the new ones and the old ones over by id. The read is outside the
// block, but the block is guarded by the worker's place (below), and every
// rollup write is such a block: one that landed in between moved the place,
// and this one is refused.
//
// Exactly once in effect. The stream hands every write over at least once
// -- the worker may crash after its block lands and before it knows -- so
// the worker keeps its place in the database, in `rollup_state`, and moves
// it in the same block as the counts:
//
//   set rollup_state {seq: <next>, ...} where name = "rollups" and seq = <prev> require 1
//
// A page applied twice finds `seq` moved and is refused whole (412), so no
// event is counted twice, and none is skipped: the place moves only with
// the counts of every event before it. Each event is a write numbered once
// by the change counter, so the change number is the event's key here; the
// events' own ids (`eid`, `@unique`) have kept a beacon sent twice from
// being written twice before the stream ever saw it.
import type { FenecHttp } from '@fenecdb/web/client';
import { FenecError } from '@fenecdb/web/client';
import { OPERATOR_TOKEN, tenantUrl } from './config.ts';
import { db } from './db.ts';
import { dayOf, HOUR, iso, MINUTE, minuteOf, weekOf } from './time.ts';

export interface RawEvent {
  name: string;
  user: string;
  path: string;
  ref: string;
  country: string;
  device: string;
  browser: string;
  at: number;
}

/** The fields the filters count by, kept a day each in `day_dims`. */
export const DIMS = ['country', 'device', 'browser'] as const;

/** What a set of events adds to the rollups. */
export interface Fold {
  minutes: Map<string, { at: number; name: string; n: number }>;
  days: Map<string, { day: number; name: string; n: number }>;
  pages: Map<string, { day: number; path: string; n: number }>;
  refs: Map<string, { day: number; ref: string; n: number }>;
  dims: Map<string, { day: number; dim: string; value: string; n: number }>;
  /** The first time of each day a visitor did each event that is not a pageview. */
  firsts: Map<string, { day: number; name: string; user: string; at: number }>;
  dayUsers: Map<string, { day: number; user: string }>;
  weekly: Map<string, { week: number; user: string }>;
  /** Each visitor's first and last day among these events. */
  first: Map<string, number>;
  last: Map<string, number>;
  events: number;
  /** The latest day among them. */
  maxDay: number;
}

export function fold(events: Iterable<RawEvent>): Fold {
  const f: Fold = {
    minutes: new Map(),
    days: new Map(),
    pages: new Map(),
    refs: new Map(),
    dims: new Map(),
    firsts: new Map(),
    dayUsers: new Map(),
    weekly: new Map(),
    first: new Map(),
    last: new Map(),
    events: 0,
    maxDay: 0,
  };
  const add = <V extends { n: number }>(m: Map<string, V>, key: string, make: () => V) => {
    const v = m.get(key);
    if (v) v.n++;
    else m.set(key, make());
  };
  for (const e of events) {
    f.events++;
    const minute = minuteOf(e.at);
    const day = dayOf(e.at);
    const week = weekOf(e.at);
    if (day > f.maxDay) f.maxDay = day;
    add(f.minutes, `${iso(minute)}|${e.name}`, () => ({ at: minute, name: e.name, n: 1 }));
    add(f.days, `${iso(day)}|${e.name}`, () => ({ day, name: e.name, n: 1 }));
    if (e.name === 'pageview') {
      add(f.pages, `${iso(day)}|${e.path}`, () => ({ day, path: e.path, n: 1 }));
      add(f.refs, `${iso(day)}|${e.ref}`, () => ({ day, ref: e.ref, n: 1 }));
    } else {
      const k = `${iso(day)}|${e.name}|${e.user}`;
      const x = f.firsts.get(k);
      if (!x) f.firsts.set(k, { day, name: e.name, user: e.user, at: e.at });
      else if (e.at < x.at) x.at = e.at;
    }
    for (const d of DIMS) add(f.dims, `${iso(day)}|${d}|${e[d]}`, () => ({ day, dim: d, value: e[d], n: 1 }));
    const du = `${iso(day)}|${e.user}`;
    if (!f.dayUsers.has(du)) f.dayUsers.set(du, { day, user: e.user });
    const wu = `${iso(week)}|${e.user}`;
    if (!f.weekly.has(wu)) f.weekly.set(wu, { week, user: e.user });
    const first = f.first.get(e.user);
    if (first === undefined || day < first) f.first.set(e.user, day);
    if (day > (f.last.get(e.user) ?? -Infinity)) f.last.set(e.user, day);
  }
  return f;
}

type Statement = readonly [string, unknown[]];

/** A row of `visitors`: its id, and the visitor's first and last day. */
export interface Visitor {
  id: number;
  first: number;
  last: number;
}

/** At most this many documents a statement: a statement's text grows with them. */
const CHUNK = 500;

/** `put <c> [docs] if absent`, the documents' fields as parameters, in chunks. */
function puts(collection: string, fields: string[], rows: unknown[][], ifAbsent = true): Statement[] {
  const out: Statement[] = [];
  for (let i = 0; i < rows.length; i += CHUNK) {
    const part = rows.slice(i, i + CHUNK);
    const docs = part.map((_, j) => `{${fields.map((f, k) => `${f}: $${j * fields.length + k + 1}`).join(', ')}}`);
    out.push([`put ${collection} [${docs.join(', ')}]${ifAbsent ? ' if absent' : ''}`, part.flat()]);
  }
  return out;
}

/** The rows a page's keys already have, by key: their ids, and a counter's `n` or a first time's `at`. */
export type Existing = Record<string, Map<string, { id: number; n: number; at: number }>>;

/** The counters' collections, the field each is keyed in time by, and its labels. */
export const COUNTERS = [
  ['minutes', 'at', ['name'], 'minutes'],
  ['days', 'day', ['name'], 'days'],
  ['day_pages', 'day', ['path'], 'pages'],
  ['day_refs', 'day', ['ref'], 'refs'],
  ['day_dims', 'day', ['dim', 'value'], 'dims'],
] as const;

/**
 * Counters by key. With the rows the keys have already (`ex`, read before
 * the block), two statements: the new rows written with their counts, the
 * old ones written over by id with theirs added -- what a key's own `set`
 * did in a statement each, a page of a backfill touching a thousand keys.
 * Without, the statements need read nothing first: each key made at zero if
 * missing, then added to.
 */
function counters(collection: string, time: string, labels: readonly string[], m: Map<string, { n: number } & Record<string, unknown>>, ex?: Existing): Statement[] {
  const fields = ['key', time, ...labels, 'n'];
  const row = (key: string, v: { n: number } & Record<string, unknown>, n: number) => [key, iso(v[time] as number), ...labels.map((l) => v[l]), n];
  if (!ex) {
    const out = puts(collection, fields, [...m].map(([k, v]) => row(k, v, 0)));
    for (const [key, v] of m) out.push([`set ${collection} {n: n + $2} where key = $1`, [key, v.n]]);
    return out;
  }
  const had = ex[collection] ?? new Map();
  const fresh = [...m].filter(([k]) => !had.has(k)).map(([k, v]) => row(k, v, v.n));
  const old = [...m].filter(([k]) => had.has(k)).map(([k, v]) => [had.get(k)!.id, ...row(k, v, had.get(k)!.n + v.n)]);
  return [...puts(collection, fields, fresh, false), ...puts(collection, ['id', ...fields], old, false)];
}

/**
 * What a page changes in `cohorts`, the visitors of each cohort who came in
 * each week: one for each week row the page makes (none already holds its
 * key, `had`), at its visitor's cohort; and for a visitor whose first day
 * the page moves into another week, each week row of theirs already held
 * (`movedWeeks`) taken from the old cohort and given to the new.
 */
export function cohortDeltas(
  f: Fold,
  known: Map<string, number>,
  had: Map<string, unknown>,
  movedWeeks: Map<string, number[]>,
): Map<string, { cohort: number; week: string; n: number }> {
  const out = new Map<string, { cohort: number; week: string; n: number }>();
  const add = (cohort: number, week: number, n: number) => {
    const k = `${iso(cohort)}|${iso(week)}`;
    const v = out.get(k);
    if (v) v.n += n;
    else out.set(k, { cohort, week: iso(week), n });
  };
  const firstOf = (u: string) => Math.min(known.get(u) ?? Infinity, f.first.get(u) ?? Infinity);
  for (const [k, v] of f.weekly) if (!had.has(k)) add(weekOf(firstOf(v.user)), v.week, 1);
  for (const [user, weeks] of movedWeeks) {
    for (const w of weeks) {
      add(weekOf(known.get(user) as number), w, -1);
      add(weekOf(firstOf(user)), w, 1);
    }
  }
  for (const [k, v] of out) if (v.n === 0) out.delete(k);
  return out;
}

/** The visitors whose first day this page moves into an earlier week. */
export function movedCohorts(f: Fold, known: Map<string, number>): string[] {
  return [...f.first].filter(([u, d]) => known.has(u) && weekOf(d) < weekOf(known.get(u) as number)).map(([u]) => u);
}

/**
 * The block that applies `f`, guarded by the worker's place: `prev` is the
 * change the rollups stand at, `next` the one they will. `known` is the
 * first day `visitors` holds for each of the page's visitors it has seen
 * before: a week row is written with its visitor's cohort, the week of that
 * first day, and a visitor whose first day this page moves back has it, and
 * the cohort of every week row of theirs, moved with it.
 */
export function block(
  f: Fold,
  prev: number,
  next: number,
  known: Map<string, number>,
  ex?: Existing,
  cohorts?: Map<string, { cohort: number; week: string; n: number }>,
  seen?: Map<string, Visitor>,
): Statement[] {
  const out: Statement[] = [
    [
      'set rollup_state {seq: $2, events: events + $3, day: greatest(day, $4), at: now()} where name = "rollups" and seq = $1 require 1',
      [prev, next, f.events, iso(f.maxDay)],
    ],
  ];
  for (const [c, time, labels, field] of COUNTERS) out.push(...counters(c, time, labels, f[field] as Map<string, { n: number } & Record<string, unknown>>, ex));
  // A first time made if missing, and moved back if this page saw an earlier one.
  const firsts = [...f.firsts].map(([k, v]) => [k, iso(v.day), v.name, v.user, iso(v.at)]);
  if (ex) {
    const had = ex.firsts ?? new Map();
    out.push(...puts('firsts', ['key', 'day', 'name', 'user', 'at'], firsts.filter(([k]) => !had.has(k as string)), false));
    const moved = [...f.firsts].filter(([k, v]) => had.has(k) && v.at < had.get(k)!.at);
    out.push(...puts('firsts', ['id', 'key', 'day', 'name', 'user', 'at'], moved.map(([k, v]) => [had.get(k)!.id, k, iso(v.day), v.name, v.user, iso(v.at)]), false));
  } else {
    out.push(...puts('firsts', ['key', 'day', 'name', 'user', 'at'], firsts));
    for (const [k, v] of f.firsts) out.push(['set firsts {at: $2} where key = $1 and at > $2', [k, iso(v.at)]]);
  }
  out.push(...puts('day_users', ['key', 'day', 'user'], [...f.dayUsers].map(([k, v]) => [k, iso(v.day), v.user])));
  const firstOf = (u: string) => Math.min(known.get(u) ?? Infinity, f.first.get(u) ?? Infinity);
  out.push(...puts('weekly', ['key', 'week', 'user', 'cohort'], [...f.weekly].map(([k, v]) => [k, iso(v.week), v.user, iso(weekOf(firstOf(v.user)))])));
  // A visitor's first and last day: new ones made, returning ones whose
  // days moved written over by id. `last` is what counts the visitors of a
  // range that ends now (`last >= day`), with no distinct count.
  const lastOf = (u: string) => f.last.get(u) as number;
  out.push(...puts('visitors', ['user', 'first', 'last'], [...f.first].filter(([u]) => !known.has(u)).map(([u, d]) => [u, iso(d), iso(lastOf(u))])));
  if (seen) {
    const moved: unknown[][] = [];
    for (const [u, d] of f.first) {
      const v = seen.get(u);
      if (!v) continue;
      const first = Math.min(v.first, d);
      const last = Math.max(v.last, lastOf(u));
      if (first !== v.first || last !== v.last) moved.push([v.id, u, iso(first), iso(last)]);
      if (weekOf(first) !== weekOf(v.first)) out.push(['set weekly {cohort: $2} where user = $1', [u, iso(weekOf(first))]]);
    }
    out.push(...puts('visitors', ['id', 'user', 'first', 'last'], moved, false));
  }
  if (cohorts) out.push(...counters('cohorts', 'cohort', ['week'], cohorts, ex));
  return out;
}

export interface WorkerOptions {
  site: string;
  /** Events a page of the stream, at most. */
  limit?: number;
  /** How long a read with nothing in it waits for a write, ms. */
  wait?: number;
  /** Called after each block lands: the change the rollups stand at. */
  onApplied?: (seq: number, events: number) => void;
  onError?: (e: unknown) => void;
}

interface Change {
  seq: number;
  collection: string;
  op: string;
  doc?: { name: string; user: string; path: string; ref: string; country: string; device: string; browser: string; at: string };
}

/** One site's rollups, kept from its change stream. */
export class RollupWorker {
  readonly site: string;
  readonly #db: FenecHttp;
  #stopped = false;
  #pulse = { checked: 0, active: -1, views: -1, minutes: '' };
  /** The change the rollups stand at, as `rollup_state` holds it. */
  seq = 0;
  /** Where the next read starts: past the worker's own writes too. */
  cursor = 0;
  applied = 0;
  rebuilds = 0;
  /** Pages refused because another block had moved the place. */
  refused = 0;
  /** Writes the last page held, events or not: 0 at the stream's end. */
  lines = 0;

  constructor(readonly opts: WorkerOptions) {
    this.site = opts.site;
    this.#db = db(opts.site, 'operator');
  }

  /** Reads the worker's place, making it at the first change if it is new. */
  async open(): Promise<void> {
    await this.#db.run('put rollup_state {name: "rollups", seq: 0, events: 0, day: $1, at: now()} if absent', [iso(0)]);
    const [s] = await this.#db.rows('get rollup_state where name = "rollups" limit 1 require 1');
    this.seq = Number(s.seq);
    this.cursor = this.seq;
  }

  /** The rows the page's counters and first times have already, read side by side. */
  async existing(f: Fold): Promise<Existing> {
    const ex: Existing = {};
    const read = async (c: string, value: string, keys: string[]) => void (ex[c] = await this.held(c, value, keys));
    await Promise.all([
      ...COUNTERS.map(([c, , , field]) => read(c, 'n', [...f[field].keys()])),
      read('firsts', 'at', [...f.firsts.keys()]),
      read('weekly', 'cohort', [...f.weekly.keys()]),
    ]);
    return ex;
  }

  /** The rows of `c` holding `keys`, by key: id and `value`, 500 keys to a statement. */
  async held(c: string, value: string, keys: string[]): Promise<Map<string, { id: number; n: number; at: number }>> {
    const m = new Map<string, { id: number; n: number; at: number }>();
    for (let i = 0; i < keys.length; i += 500) {
      const part = keys.slice(i, i + 500);
      const rows = await this.#db.rows(`get ${c} select id, key, ${value} as v where key in [${part.map((_, j) => `$${j + 1}`).join(', ')}] limit ${part.length}`, part);
      for (const r of rows) m.set(r.key as string, { id: Number(r.id), n: Number(r.v), at: Date.parse(r.v as string) });
    }
    return m;
  }

  /** What `visitors` holds of each of `users` it has: id, first and last day, 500 to a statement. */
  async seen(users: string[]): Promise<Map<string, Visitor>> {
    const out = new Map<string, Visitor>();
    for (let i = 0; i < users.length; i += 500) {
      const part = users.slice(i, i + 500);
      const rows = await this.#db.rows(`get visitors select id, user, first, last where user in [${part.map((_, j) => `$${j + 1}`).join(', ')}] limit ${part.length}`, part);
      for (const r of rows) out.set(r.user as string, { id: Number(r.id), first: Date.parse(r.first as string), last: Date.parse(r.last as string) });
    }
    return out;
  }

  /**
   * Reads a page of the stream from the cursor, and applies its events as
   * one guarded block. Answers how many events it applied. `crash: 'after'`
   * lands the block and forgets it did, as a crash before the worker
   * noted it would.
   */
  async step(opts: { crash?: 'after'; wait?: number } = {}): Promise<number> {
    const q = `?since=${this.cursor}&limit=${this.opts.limit ?? 2000}&wait=${opts.wait ?? this.opts.wait ?? 1000}`;
    const res = await fetch(`${tenantUrl(this.site)}/_changes${q}`, { headers: { authorization: `Bearer ${OPERATOR_TOKEN}` } });
    if (res.status === 410) {
      // The node no longer keeps the writes the worker stands at: it fell
      // further behind than --replication-buffer, or the node restarted.
      // Rebuild what those writes could have touched from the raw events.
      await res.body?.cancel();
      await this.rebuild(Date.now() - 2 * HOUR);
      return 0;
    }
    if (!res.ok) throw new Error(`/_changes: ${res.status} ${await res.text()}`);
    const next = Number(res.headers.get('fenec-next') ?? this.cursor);
    const events: RawEvent[] = [];
    this.lines = 0;
    for (const line of (await res.text()).split('\n')) {
      if (!line) continue;
      this.lines++;
      const c = JSON.parse(line) as Change;
      // The sweeper's deletes, the worker's own writes: not events.
      if (c.collection !== 'events' || c.op !== 'put' || !c.doc) continue;
      const d = c.doc;
      events.push({ name: d.name, user: d.user, path: d.path, ref: d.ref ?? '', country: d.country, device: d.device, browser: d.browser, at: Date.parse(d.at) });
    }
    if (!events.length) {
      this.cursor = Math.max(this.cursor, next);
      await this.pulse();
      return 0;
    }
    const f = fold(events);
    // Read outside the block: one worker writes a site's rollups, and a
    // second one racing it has its block refused by the guard, then reads again.
    const [seen, ex] = await Promise.all([this.seen([...f.first.keys()]), this.existing(f)]);
    const known = new Map([...seen].map(([u, v]) => [u, v.first]));
    // A visitor whose first week moves back (a late event) takes the weeks
    // they came already from one cohort to the other.
    const movedWeeks = new Map<string, number[]>();
    for (const u of movedCohorts(f, known)) {
      movedWeeks.set(u, (await this.#db.rows('get weekly select week where user = $1 limit 10000', [u])).map((r) => Date.parse(r.week)));
    }
    const cohorts = cohortDeltas(f, known, ex.weekly, movedWeeks);
    ex.cohorts = await this.held('cohorts', 'n', [...cohorts.keys()]);
    try {
      await this.#db.batch(block(f, this.seq, next, known, ex, cohorts, seen));
    } catch (e) {
      if (e instanceof FenecError && e.status === 412 && e.at === 0) {
        // Another block moved the place first -- this worker's own, landed
        // before a crash, or a second worker's. Nothing of this one landed:
        // read where the rollups stand and go on from there.
        this.refused++;
        await this.open();
        return 0;
      }
      throw e;
    }
    if (opts.crash === 'after') return 0;
    this.seq = next;
    this.cursor = Math.max(this.cursor, next);
    this.applied += f.events;
    this.opts.onApplied?.(next, f.events);
    await this.pulse();
    return f.events;
  }

  /**
   * The rollups from `since`'s week on made again from the raw events: read
   * in one block, which names the change it saw (`Fenec-Seq`), and written
   * in one block that moves the worker's place there. The raw events are
   * kept 30 days, so this reaches back that far at most.
   */
  async rebuild(since: number): Promise<void> {
    const from = weekOf(since);
    const p = [iso(from)];
    const reads: Statement[] = [
      ['get events select bucket(at, 1m) as m, name, count(*) as n where at >= $1 group m, name', p],
      ['get events select bucket(at, 1d) as day, name, count(*) as n where at >= $1 group day, name', p],
      ['get events select bucket(at, 1d) as day, path, count(*) as n where at >= $1 and name = "pageview" group day, path', p],
      ['get events select bucket(at, 1d) as day, ref, count(*) as n where at >= $1 and name = "pageview" group day, ref', p],
      ['get events select bucket(at, 1d) as day, user, count(*) as n where at >= $1 group day, user', p],
      ['get events select bucket(at, 1w) as week, user, count(*) as n where at >= $1 group week, user', p],
      ['get events select user, min(bucket(at, 1d)) as first, max(bucket(at, 1d)) as last where at >= $1 group user', p],
      ['get events select max(at) as last where at >= $1', p],
      ['get rollup_state where name = "rollups" limit 1 require 1', []],
      ...DIMS.map((d): Statement => [`get events select bucket(at, 1d) as day, ${d} as value, count(*) as n where at >= $1 group day, value`, p]),
      ['get events select bucket(at, 1d) as day, name, user, min(at) as at where at >= $1 and name != "pageview" group day, name, user', p],
    ];
    const r = await this.#db.batch(reads);
    const rows = r.results.map((x) => (x as { rows: Record<string, unknown>[] }).rows);
    const seq = r.seq ?? 0;
    const prev = Number(rows[8][0].seq);
    const t = (v: unknown) => Date.parse(v as string);
    const key = (a: unknown, b: unknown) => `${iso(t(a))}|${b}`;
    const last = rows[7][0]?.last ? dayOf(t(rows[7][0].last)) : from;
    const w: Statement[] = [
      [
        'set rollup_state {seq: $2, day: greatest(day, $3), at: now()} where name = "rollups" and seq = $1 require 1',
        [prev, seq, iso(last)],
      ],
      ['del minutes where at >= $1', p],
      ['del days where day >= $1', p],
      ['del day_pages where day >= $1', p],
      ['del day_refs where day >= $1', p],
      ['del day_users where day >= $1', p],
      ['del weekly where week >= $1', p],
      ['del day_dims where day >= $1', p],
      ['del firsts where day >= $1', p],
      ['del cohorts where week >= $1', p],
    ];
    w.push(...puts('minutes', ['key', 'at', 'name', 'n'], rows[0].map((x) => [key(x.m, x.name), x.m, x.name, x.n]), false));
    w.push(...puts('days', ['key', 'day', 'name', 'n'], rows[1].map((x) => [key(x.day, x.name), x.day, x.name, x.n]), false));
    w.push(...puts('day_pages', ['key', 'day', 'path', 'n'], rows[2].map((x) => [key(x.day, x.path), x.day, x.path, x.n]), false));
    w.push(...puts('day_refs', ['key', 'day', 'ref', 'n'], rows[3].map((x) => [key(x.day, x.ref), x.day, x.ref, x.n]), false));
    w.push(...puts('day_users', ['key', 'day', 'user'], rows[4].map((x) => [key(x.day, x.user), x.day, x.user]), false));
    // A week row's cohort is its visitor's first week: from the range, or
    // from `visitors` for one first seen before it (not deleted above).
    // Every visitor's days, from what `visitors` held and the range's events.
    const seen = await this.seen(rows[6].map((x) => x.user as string));
    const firstIn = new Map(rows[6].map((x) => [x.user as string, t(x.first)]));
    const cohort = (u: string) => iso(weekOf(Math.min(seen.get(u)?.first ?? Infinity, firstIn.get(u) ?? Infinity)));
    w.push(...puts('weekly', ['key', 'week', 'user', 'cohort'], rows[5].map((x) => [key(x.week, x.user), x.week, x.user, cohort(x.user as string)]), false));
    const counts = new Map<string, [string, string, number]>();
    for (const x of rows[5]) {
      const c = cohort(x.user as string);
      const k = `${c}|${iso(t(x.week))}`;
      const v = counts.get(k) ?? [c, iso(t(x.week)), 0];
      v[2]++;
      counts.set(k, v);
    }
    w.push(...puts('cohorts', ['key', 'cohort', 'week', 'n'], [...counts].map(([k, v]) => [k, ...v]), false));
    DIMS.forEach((d, i) =>
      w.push(...puts('day_dims', ['key', 'day', 'dim', 'value', 'n'], rows[9 + i].map((x) => [`${iso(t(x.day))}|${d}|${x.value}`, x.day, d, x.value, x.n]), false)),
    );
    w.push(...puts('firsts', ['key', 'day', 'name', 'user', 'at'], rows[12].map((x) => [`${iso(t(x.day))}|${x.name}|${x.user}`, x.day, x.name, x.user, x.at]), false));
    w.push(...puts('visitors', ['user', 'first', 'last'], rows[6].filter((x) => !seen.has(x.user as string)).map((x) => [x.user, x.first, x.last]), false));
    w.push(
      ...puts(
        'visitors',
        ['id', 'user', 'first', 'last'],
        rows[6]
          .filter((x) => seen.has(x.user as string))
          .map((x) => {
            const v = seen.get(x.user as string) as Visitor;
            return [v.id, x.user, iso(Math.min(v.first, t(x.first))), iso(Math.max(v.last, t(x.last)))];
          }),
        false,
      ),
    );
    try {
      await this.#db.batch(w);
    } catch (e) {
      if (e instanceof FenecError && e.status === 412 && e.at === 0) {
        this.refused++;
        await this.open();
        return;
      }
      throw e;
    }
    this.rebuilds++;
    this.seq = seq;
    this.cursor = seq;
  }

  /**
   * The dashboard's "now": visitors in the last five minutes and pageviews
   * a minute for the last half hour, written to `pulse` when they changed
   * -- at most every two seconds, and every ten while nothing arrives, so
   * the count falls as visitors leave.
   */
  async pulse(force = false): Promise<void> {
    const now = Date.now();
    const quiet = now - this.#pulse.checked;
    if (!force && quiet < 2000) return;
    this.#pulse.checked = now;
    const since = minuteOf(now) - 29 * MINUTE;
    // Two reads side by side, the page's time the slower one's.
    const [a, m] = await Promise.all([
      this.#db.rows('get events select count(distinct user) as active where at >= $1', [iso(now - 5 * MINUTE)]),
      this.#db.rows('get events select bucket(at, 1m) as m, count(*) as n where at >= $1 and name = "pageview" group m', [iso(since)]),
    ]);
    const active = Number(a[0]?.active ?? 0);
    const counts = new Map(m.map((x) => [Date.parse(x.m as string), Number(x.n)]));
    const minutes = Array.from({ length: 30 }, (_, i) => counts.get(since + i * MINUTE) ?? 0);
    const views = minutes.reduce((s, n) => s + n, 0);
    const text = JSON.stringify(minutes);
    if (active === this.#pulse.active && views === this.#pulse.views && text === this.#pulse.minutes) return;
    this.#pulse = { checked: now, active, views, minutes: text };
    await this.#db.batch([
      ['put pulse {name: "now", active: 0, views: 0, minutes: [], at: now()} if absent', []],
      ['set pulse {active: $1, views: $2, minutes: $3, at: now()} where name = "now"', [active, views, minutes]],
    ]);
  }

  /**
   * Opens and applies pages until one holds no write for `quiet` ms: the
   * stream's end, for now. The stream hands over only what an fsync
   * covered, so a write made a moment ago under `--sync 250` is not in it
   * yet: an empty page waits longer than that before it counts as the end.
   */
  async catchUp(quiet = 600): Promise<void> {
    await this.open();
    for (;;) {
      await this.step({ wait: 0 });
      if (this.lines > 0) continue;
      await this.step({ wait: quiet });
      if (this.lines === 0) break;
    }
    await this.pulse(true);
  }

  /** Runs until `stop()`. */
  async run(): Promise<void> {
    for (;;) {
      try {
        await this.open();
        break;
      } catch (e) {
        this.opts.onError?.(e);
        if (this.#stopped) return;
        await new Promise((r) => setTimeout(r, 1000));
      }
    }
    let quiet = 0;
    while (!this.#stopped) {
      try {
        const n = await this.step();
        quiet = n ? 0 : quiet + 1;
        if (quiet >= 10) {
          quiet = 0;
          await this.pulse(true);
        }
      } catch (e) {
        this.opts.onError?.(e);
        await new Promise((r) => setTimeout(r, 500));
      }
    }
  }

  stop() {
    this.#stopped = true;
  }
}
