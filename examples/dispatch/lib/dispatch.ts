// The queue: an order is a job, a courier claims the nearest ready one,
// holds it under a lease it extends as it rides, and acks it by delivering.
// Every step is one statement or one /batch under the courier's own token,
// so policy.txt -- not this code -- decides what a courier may write.
//
// "Claim the nearest" is the one step FenecQL cannot say in one statement.
// A claim is `set ... where ... order <field> limit n returning *`: its
// `order` takes fields, and `near`, which orders by distance, is a `get`'s
// alone. And `set ... where id in (get orders select id ... near pickup $1
// limit 1)`, which would pick the nearest under the write lock, refuses its
// point: a point inside an inner `get` is read as a vector's f32s and
// refused ("a point is [lon, lat] ... found [27.01, 38.36]"). So a claim is:
//
//   1. the nearest ready orders within the radius, a read (`near pickup`),
//      with the app's token -- a courier's own reads only their orders;
//   2. one /batch that takes the first of them still ready, in that order:
//      a `set` each, each also asking that none before it took one
//      (`not ($claim in (get orders select claim where claim = $claim))`),
//      all under the one write lock;
//   3. none near and ready: a claim of the oldest ready order within the
//      radius, held at the server until one is placed (`Fenec-Wait`).
//
// Each `set` answers the order it took (`returning *`). A courier reads
// only the orders they carry, which a ready order is not until the claim
// is written: the read rules are asked of the order as the claim leaves it,
// so it is answered as claimed. (Asked of it as it stood, the claim picked
// nothing and said nothing, and each batch read the order back by its
// claim token.)
import { CLAIM_RADIUS_M, CLAIM_WAIT_MS, LEASE_MS, MAX_ATTEMPTS } from './config';
import { appDb, asUser, FenecError, retrying, rows, type Db } from './db';
import type { Point } from './city';
import type { Who } from './tokens';
import { randomUUID } from 'node:crypto';

export interface Job {
  id: number;
  ref: string;
  merchant: string;
  customer: string;
  pickup: Point;
  pickup_name: string;
  dropoff: Point;
  dropoff_addr: string;
  items: number;
  fee: number;
  status: string;
  attempts: number;
  claim: string;
  courier: string;
  run_at: string | null;
  promised: string;
  created: string;
  picked_at: string | null;
  delivered_at: string | null;
  driven: number | null;
  courier_loc: Point | null;
  error: string | null;
}

/** How many of the nearest ready orders a claim tries, in order. */
export const CANDIDATES = 6;

/** The lease a claim takes and each heartbeat extends; the lease tests shorten it. */
export const queue = { leaseMs: LEASE_MS };

/** What claims cost: batches sent, batches that took an order, held claims and what they took. */
export const claimStats = { batches: 0, won: 0, held: 0, heldWon: 0 };

const NEAREST =
  'get orders select id where run_at <= now() and distance(pickup, $1) <= $2 near pickup $1 limit $3';

// Each statement of the claim's /batch: this candidate, if it is still
// ready and no statement before it in the batch took one, answered as taken.
const TAKE_IF_FIRST =
  'set orders {status: "assigned", courier: $1, claim: $2, run_at: now() + $3, attempts: attempts + 1} ' +
  'where id = $4 and run_at <= now() and not ($2 in (get orders select claim where claim = $2)) returning *';

// The oldest ready order within the radius: what a courier waits on.
const TAKE_OLDEST_NEAR =
  'set orders {status: "assigned", courier: $1, claim: $2, run_at: now() + $3, attempts: attempts + 1} ' +
  'where run_at <= now() and distance(pickup, $4) <= $5 order run_at limit 1 returning *';

/** The order a statement of a claim's batch answered, if it took one. */
const taken = (result: unknown): Job | undefined => (result as { rows?: Job[] }).rows?.[0];

export interface ClaimOptions {
  radius?: number;
  /** Milliseconds to wait at the server when nothing is near; 0 answers at once. */
  wait?: number;
  signal?: AbortSignal;
  /** The connection the candidates are read with: the app's, by default. */
  reader?: Db;
}

export interface Claimed {
  job: Job;
  /** How it was found: among the nearest, or by waiting. */
  how: 'nearest' | 'waited';
  /** Statements the batch tried before one took it. */
  tried: number;
}

/** A claim's own token: one claim of one order, so a lapsed one is told apart. */
export const claimToken = (courier: string) => `${courier}:${randomUUID().slice(0, 8)}`;

/**
 * The nearest ready order within the radius, claimed for `who` under a
 * lease; with `wait`, held at the server until one comes. `null` when none
 * came. An order whose attempts pass the limit is a dead letter: the
 * courier who drew it marks it so and claims again.
 */
export async function claimNearest(who: Who, at: Point, opts: ClaimOptions = {}): Promise<Claimed | null> {
  const db = asUser(who);
  const radius = opts.radius ?? CLAIM_RADIUS_M;
  for (let round = 0; round < 3; round++) {
    const reader = opts.reader ?? appDb(who.company!);
    const ids = (await rows<{ id: number }>(reader, NEAREST, [at, radius, CANDIDATES])).map((r) => r.id);
    let got: Claimed | null = null;
    if (ids.length) {
      const claim = claimToken(who.sub);
      // Refused for the company's rate (429) before it ran: sent again.
      const r = await retrying(() =>
        db.batch(ids.map((id): [string, unknown[]] => [TAKE_IF_FIRST, [who.sub, claim, queue.leaseMs, id]])),
      );
      const i = r.results.findIndex((x) => taken(x) !== undefined);
      claimStats.batches++;
      if (i >= 0) {
        claimStats.won++;
        got = { job: taken(r.results[i])!, how: 'nearest', tried: i + 1 };
      }
    }
    if (!got && opts.wait) {
      // Nothing near is ready: wait at the server for the oldest one within
      // the radius, the batch held while its `set` writes nothing. A held
      // claim is a stream of the company's quota.
      const claim = claimToken(who.sub);
      const r = await retrying(() =>
        db.batch([[TAKE_OLDEST_NEAR, [who.sub, claim, queue.leaseMs, at, radius]]], {
          wait: opts.wait ?? CLAIM_WAIT_MS,
          signal: opts.signal,
        }),
      );
      const job = taken(r.results[0]);
      claimStats.held++;
      if (job) {
        claimStats.heldWon++;
        got = { job, how: 'waited', tried: 0 };
      }
    }
    if (!got) return null;
    if (got.job.attempts > MAX_ATTEMPTS) {
      await deadLetter(who, got.job, `claimed ${got.job.attempts} times without a delivery`);
      continue;
    }
    return got;
  }
  return null;
}

/** The courier has the parcel: picked up, the lease running on. */
export async function pickUp(who: Who, job: Pick<Job, 'id' | 'claim'>): Promise<boolean> {
  return ok(() =>
    asUser(who).run(
      'set orders {status: "picked", picked_at: now(), run_at: now() + $3} where id = $1 and claim = $2 require 1',
      [job.id, job.claim, queue.leaseMs],
    ),
  );
}

/**
 * Where the courier is, every few seconds: their position written over,
 * and the lease of the order they carry moved on with their point on it --
 * one /batch, so one write of the company's quota. `false` when the lease
 * was lost: it lapsed and another courier claimed the order.
 */
export async function heartbeat(
  who: Who,
  p: { at: Point; state: string; heading?: number; vehicle?: string; job?: Pick<Job, 'id' | 'claim'> | null },
): Promise<{ kept: boolean; throttled?: number }> {
  const statements: [string, unknown[]][] = [
    [
      'put positions {courier: $1, loc: $2, seen: now(), state: $3, heading: $4, job: $5, vehicle: $6} ' +
        'if absent else set {loc: new.loc, seen: new.seen, state: new.state, heading: new.heading, job: new.job}',
      [who.sub, p.at, p.state, p.heading ?? 0, p.job?.id ?? null, p.vehicle ?? null],
    ],
  ];
  if (p.job) {
    statements.push([
      'set orders {run_at: now() + $3, courier_loc: $4} where id = $1 and claim = $2',
      [p.job.id, p.job.claim, queue.leaseMs, p.at],
    ]);
  }
  // A beat the company's rate refuses is skipped, not sent again: the next
  // one is due in seconds and says more, and the lease has room for it.
  let r;
  try {
    r = await asUser(who).batch(statements);
  } catch (e) {
    if (e instanceof FenecError && e.status === 429) return { kept: true, throttled: e.retryAfter ?? 1 };
    throw e;
  }
  const kept = !p.job || (r.results[1] as { affected?: number }).affected === 1;
  return { kept };
}

/** Delivered: the ack. `false` when the lease was lost before it. */
export async function deliver(who: Who, job: Pick<Job, 'id' | 'claim'>, driven: number): Promise<boolean> {
  return ok(() =>
    asUser(who).run(
      'set orders {status: "delivered", delivered_at: now(), run_at: null, claim: null, driven: $3, courier_loc: null} ' +
        'where id = $1 and claim = $2 require 1',
      [job.id, job.claim, Math.round(driven)],
    ),
  );
}

/**
 * The courier cannot carry it -- the merchant closed, the bike broke: back
 * to the queue after a backoff that grows with its attempts, the square of
 * them in seconds, and dead after the last.
 */
export async function giveBack(who: Who, job: Pick<Job, 'id' | 'claim'>, why: string): Promise<boolean> {
  return ok(() =>
    asUser(who).run(
      `set orders {status: case when attempts >= ${MAX_ATTEMPTS} then "dead" else "ready" end, ` +
        `run_at: case when attempts >= ${MAX_ATTEMPTS} then null else now() + 1000 * attempts * attempts end, ` +
        'claim: null, error: $3} where id = $1 and claim = $2 require 1',
      [job.id, job.claim, why],
    ),
  );
}

/** An order no claim will take again, for a dispatcher to look at. */
export async function deadLetter(who: Who, job: Pick<Job, 'id' | 'claim'>, why: string): Promise<boolean> {
  return ok(() =>
    asUser(who).run(
      'set orders {status: "dead", run_at: null, claim: null, error: $3} where id = $1 and claim = $2 require 1',
      [job.id, job.claim, why],
    ),
  );
}

/**
 * A write whose `require` was not met is a lease lost, not an error; one
 * the company's rate refused (429) never ran, and is sent again.
 */
async function ok(f: () => Promise<unknown>): Promise<boolean> {
  try {
    await retrying(f);
    return true;
  } catch (e) {
    if (e instanceof FenecError && e.status === 412) return false;
    throw e;
  }
}
