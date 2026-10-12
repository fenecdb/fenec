// Merrow in motion: customers placing orders in the day's waves, and each
// company's couriers riding the streets to claim, carry and deliver them --
// every courier through its own token, as its phone would. Time runs
// `speed` times faster than the clock, rides and order rates alike, so a
// dinner rush fits a demo.
import { CLAIM_RADIUS_M, CLAIM_WAIT_MS } from './config';
import { addresses, along, metresBetween, rng, route, toMetres, type Point } from './city';
import { claimNearest, deliver, giveBack, heartbeat, pickUp, type Job } from './dispatch';
import { placeOrder } from './orders';
import { merchants, people, wave, type CompanySpec, type Person } from './world';
import { FenecError } from './db';
import type { Who } from './tokens';

export interface SimOptions {
  /** How much faster than the clock the city moves. */
  speed: number;
  /** Seconds between a courier's beats, on the clock. */
  beat: number;
  /** Pin the hour of the day the order waves follow; otherwise Merrow's own (UTC+3). */
  hour?: number;
  /** Orders an hour at the busiest, before `speed`; the company's own by default. */
  peak?: number;
  /** The company's own rate times this: a busier night than its history. */
  busy?: number;
  log?: (s: string) => void;
}

export interface SimStats {
  placed: number;
  claimed: number;
  delivered: number;
  givenBack: number;
  lost: number;
  dropped: number;
  throttled: number;
  errors: number;
}

const PACE = { bike: 4.2, scooter: 6.5, car: 7.5 } as const;
const sleep = (ms: number, stop?: AbortSignal) =>
  new Promise<void>((r) => {
    const t = setTimeout(r, ms);
    stop?.addEventListener('abort', () => (clearTimeout(t), r()), { once: true });
  });

/** Merrow's hour of the day, as a number: UTC+3. */
export const merrowHour = (at = Date.now()) => ((at / 3_600_000 + 3) % 24 + 24) % 24;

/** Customers placing orders, a Poisson stream at the wave's rate. */
export async function orderStream(c: CompanySpec, o: SimOptions, stats: SimStats, stop: AbortSignal): Promise<void> {
  const r = rng(c.seed * 17 + Date.now() % 100_000);
  const shops = merchants(c);
  const buyers = people(c).filter((p) => p.role === 'customer');
  const doors = addresses();
  let n = 0;
  while (!stop.aborted) {
    const hour = o.hour ?? merrowHour();
    const perHour = (o.peak ?? c.ordersPerDay / 9) * (o.busy ?? 1) * wave(hour) * o.speed;
    const gap = (-Math.log(1 - r()) / perHour) * 3_600_000;
    await sleep(Math.min(gap, 60_000), stop);
    if (stop.aborted) break;
    const buyer = buyers[Math.floor(r() * buyers.length)];
    const home = r() < 0.8 ? buyer.home : doors[Math.floor(r() * doors.length)];
    // The nearest of a few merchants: people order from their part of town.
    let shop = shops[Math.floor(r() * shops.length)];
    for (let k = 0; k < 4; k++) {
      const s = shops[Math.floor(r() * shops.length)];
      if (metresBetween(s.loc, home.point) < metresBetween(shop.loc, home.point)) shop = s;
    }
    const metres = metresBetween(shop.loc, home.point);
    try {
      await placeOrder(
        { sub: buyer.uid, role: 'customer', company: c.slug },
        {
          ref: `${shop.code}-${Date.now().toString(36)}${(n++).toString(36)}`,
          merchant: shop,
          dropoff: home.point,
          dropoffAddr: home.label,
          items: 1 + Math.floor(r() * 4),
          fee: 250 + Math.round(metres / 10) * 5,
          promise: Math.max(5, Math.round(45 / Math.max(1, o.speed / 2))),
        },
      );
      stats.placed++;
    } catch (e) {
      stats.errors++;
      o.log?.(`${c.slug}: an order was not placed: ${(e as Error).message}`);
    }
  }
}

/** One courier's shift: claim, ride, pick up, ride, deliver, again. */
export async function courierShift(
  c: CompanySpec,
  p: Person,
  o: SimOptions,
  stats: SimStats,
  stop: AbortSignal,
): Promise<void> {
  const who: Who = { sub: p.uid, role: 'courier', company: c.slug };
  const r = rng(c.seed * 1009 + Number(p.uid.split('-').at(-1)));
  const pace = PACE[p.vehicle ?? 'bike'] * o.speed;
  let at: Point = p.home.point;
  let heading = 0;
  // A courier starts their shift a few seconds apart from the next.
  await sleep(r() * o.beat * 1000, stop);
  const beat = async (state: string, job: Job | null) => {
    try {
      const h = await heartbeat(who, { at, state, heading, vehicle: p.vehicle, job });
      if (h.throttled) {
        stats.throttled++;
        stats.dropped++;
      }
      return h.kept;
    } catch (e) {
      stats.errors++;
      o.log?.(`${p.uid}: a beat failed: ${(e as Error).message}`);
      return true;
    }
  };
  /** Rides a path, beating as it goes; false when the lease was lost on the way. */
  const ride = async (path: Point[], state: string, job: Job | null): Promise<boolean> => {
    let done = 0;
    for (;;) {
      if (stop.aborted) return false;
      await sleep(o.beat * 1000, stop);
      done += pace * o.beat;
      const step = along(path, done);
      at = step.at;
      heading = step.heading;
      const kept = await beat(state, job);
      if (!kept) return false;
      if (step.done) return true;
    }
  };
  while (!stop.aborted) {
    await beat('idle', null);
    let got;
    try {
      got = await claimNearest(who, at, { radius: CLAIM_RADIUS_M, wait: Math.min(CLAIM_WAIT_MS, o.beat * 4000), signal: stop });
    } catch (e) {
      if (stop.aborted) break;
      stats.errors++;
      o.log?.(`${p.uid}: a claim failed: ${(e as Error).message}${e instanceof FenecError ? ` (${e.status})` : ''}`);
      await sleep(2000, stop);
      continue;
    }
    if (!got) {
      // Nothing near: drift towards the middle of town, where the merchants are.
      const [x, y] = toMetres(at);
      if (Math.hypot(x - 3500, y - 3000) > 900 && r() < 0.5) {
        const toward = route(at, addresses()[Math.floor(r() * 2000) + 15_000].point).path.slice(0, 4);
        await ride(toward, 'idle', null);
      }
      continue;
    }
    stats.claimed++;
    const job = got.job;
    const toPickup = route(at, job.pickup);
    if (!(await ride(toPickup.path, 'pickup', job))) {
      stats.lost++;
      continue;
    }
    // Now and then the merchant has closed, or the order is not ready and
    // the courier gives it back for another; rarer, the phone dies here.
    if (r() < 0.02) {
      await giveBack(who, job, 'the merchant had closed');
      stats.givenBack++;
      continue;
    }
    if (r() < 0.005) {
      o.log?.(`${p.uid}: the phone died with order ${job.id}`);
      await sleep(job.run_at ? 60_000 : 60_000, stop);
      continue;
    }
    await sleep(((20 + r() * 90) * 1000) / o.speed, stop);
    if (!(await pickUp(who, job))) {
      stats.lost++;
      continue;
    }
    const toDoor = route(at, job.dropoff);
    if (!(await ride(toDoor.path, 'dropoff', job))) {
      stats.lost++;
      continue;
    }
    if (await deliver(who, job, toPickup.metres + toDoor.metres)) stats.delivered++;
    else stats.lost++;
  }
}

/** A company's whole day: its orders and its couriers, until `stop`. */
export function runCompany(c: CompanySpec, o: SimOptions, stop: AbortSignal, couriers?: number): { stats: SimStats; done: Promise<unknown> } {
  const stats: SimStats = { placed: 0, claimed: 0, delivered: 0, givenBack: 0, lost: 0, dropped: 0, throttled: 0, errors: 0 };
  // The last courier is a person's to ride from the courier page (lib/demo.ts).
  const fleet = people(c)
    .filter((p) => p.role === 'courier')
    .slice(0, Math.min(couriers ?? c.couriers, c.couriers - 1));
  const done = Promise.all([orderStream(c, o, stats, stop), ...fleet.map((p) => courierShift(c, p, o, stats, stop))]);
  return { stats, done };
}
