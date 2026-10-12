// The queue: an order is a job, a courier claims the nearest ready one,
// holds it under a lease it extends as it rides, and acks it by delivering.
// Every step is one statement or one /batch under the courier's own token,
// so policy.txt -- not this code -- decides what a courier may write.
//
// "Claim the nearest" is one statement under the courier's own token:
//
//   set orders {...} where run_at <= now() and distance(pickup, $4) <= $5
//     order distance(pickup, $4) limit 1 returning *
//
// -- the ready orders within the radius walked nearest first through the
// pickup's point index, the first taken under the write lock, and answered
// as written. It was three steps while `order` took fields alone: a read of
// the six nearest ready orders with the app's token, a /batch of a
// conditional `set` a candidate, and a held claim of the oldest within the
// radius -- two round trips, a write of the plan for every batch that lost
// its candidates, and 50 couriers at once waiting 7.6 ms p50 for each
// other's batches. With nothing near and ready, the same statement is held
// at the server (`Fenec-Wait`) and takes the nearest once one is.
//
// `returning *` answers the order taken. A courier reads only the orders
// they carry, which a ready order is not until the claim is written: the
// read rules are asked of the order as the claim leaves it, so it is
// answered as claimed.
import { CLAIM_RADIUS_M, LEASE_MS, MAX_ATTEMPTS } from './config';
import { asUser, FenecError, retrying } from './db';
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

/** The lease a claim takes and each heartbeat extends; the lease tests shorten it. */
export const queue = { leaseMs: LEASE_MS };

/** What claims cost: claims sent at once, held claims, and the orders each took. */
export const claimStats = { claims: 0, won: 0, held: 0, heldWon: 0 };

// The nearest ready order within the radius, taken and answered: what a
// courier claims at once, and waits on.
const TAKE_NEAREST =
  'set orders {status: "assigned", courier: $1, claim: $2, run_at: now() + $3, attempts: attempts + 1} ' +
  'where run_at <= now() and distance(pickup, $4) <= $5 order distance(pickup, $4) limit 1 returning *';

/** The order a statement of a claim's batch answered, if it took one. */
const taken = (result: unknown): Job | undefined => (result as { rows?: Job[] }).rows?.[0];

export interface ClaimOptions {
  radius?: number;
  /** Milliseconds to wait at the server when nothing is near; 0 answers at once. */
  wait?: number;
  signal?: AbortSignal;
}

export interface Claimed {
  job: Job;
  /**
   * How it was asked for: answered at once, or held at the server
   * (`Fenec-Wait`) -- which takes an order ready when it comes in as at
   * once, and waits only while none is.
   */
  how: 'nearest' | 'held';
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
    const take: [string, unknown[]] = [TAKE_NEAREST, [who.sub, claimToken(who.sub), queue.leaseMs, at, radius]];
    // With `wait`, one request: a held claim runs at once first and is held
    // only while it writes nothing, then run once an order within the radius
    // is ready -- placed, its lease lapsed, its backoff ended -- taking the
    // nearest then. Sent at once first and held after, each idle round was
    // two writes of the company's rate where one does. A held claim is a
    // stream of the company's quota. Refused for the rate (429) before it
    // ran: sent again.
    const held = !!opts.wait;
    const r = await retrying(() =>
      held ? db.batch([take], { wait: opts.wait, signal: opts.signal }) : db.batch([take]),
    );
    const job = taken(r.results[0]);
    if (held) claimStats.held++;
    else claimStats.claims++;
    if (!job) return null;
    if (held) claimStats.heldWon++;
    else claimStats.won++;
    const got: Claimed = { job, how: held ? 'held' : 'nearest' };
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
