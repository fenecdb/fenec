// What a dispatcher's Insights page asks, each one statement over the
// company's orders: deliveries an hour, on time or late, metres driven, the
// waves by hour of the day, and the couriers who carried the most. Merrow
// keeps UTC+3 and `bucket` has no time zone, so a local day or hour is
// `bucket(t + 3 h, ...)`: an offset written into every statement.
import { rows, type Db } from './db';

export const MERROW_OFFSET = 3 * 3_600_000;
const H = 3_600_000;

export interface Totals {
  n: number;
  on_time: number;
  ms: number | null;
  metres: number | null;
}

export interface Hourly {
  hour: string;
  n: number;
  on_time: number;
  metres: number;
}

export interface ByHour {
  h: number;
  n: number;
  on_time: number;
}

export interface Leader {
  courier: string;
  n: number;
  on_time: number;
  metres: number;
  ride: number;
}

/** The start of Merrow's day, in milliseconds. */
export function merrowMidnight(now = Date.now()): number {
  return Math.floor((now + MERROW_OFFSET) / 86_400_000) * 86_400_000 - MERROW_OFFSET;
}

export async function insights(db: Db, now = Date.now()) {
  const t0 = performance.now();
  const today = merrowMidnight(now);
  const week = today - 7 * 86_400_000;
  const [[day], [seven], hourly, byHour, leaders] = await Promise.all([
    rows<Totals>(
      db,
      'get orders select count(*) as n, sum(case when delivered_at <= promised then 1 else 0 end) as on_time, ' +
        'avg(delivered_at - created) as ms, sum(driven) as metres where delivered_at >= $1',
      [today],
    ),
    rows<Totals>(
      db,
      'get orders select count(*) as n, sum(case when delivered_at <= promised then 1 else 0 end) as on_time, ' +
        'avg(delivered_at - created) as ms, sum(driven) as metres where delivered_at >= $1 and delivered_at < $2',
      [week, today],
    ),
    rows<Hourly>(
      db,
      'get orders select bucket(delivered_at, 3600000) as hour, count(*) as n, ' +
        'sum(case when delivered_at <= promised then 1 else 0 end) as on_time, sum(driven) as metres ' +
        'where delivered_at >= $1 group hour order hour',
      [Math.floor(now / H) * H - 47 * H],
    ),
    rows<ByHour>(
      db,
      'get orders select (created + $2 - bucket(created + $2, 86400000)) / 3600000 as h, count(*) as n, ' +
        'sum(case when delivered_at <= promised then 1 else 0 end) as on_time ' +
        'where delivered_at >= $1 group h order h',
      [today - 14 * 86_400_000, MERROW_OFFSET],
    ),
    rows<Leader>(
      db,
      'get orders select courier, count(*) as n, sum(case when delivered_at <= promised then 1 else 0 end) as on_time, ' +
        'sum(driven) as metres, avg(delivered_at - picked_at) as ride where delivered_at >= $1 group courier order n desc limit 10',
      [week],
    ),
  ]);
  return { day, seven, hourly, byHour, leaders, today, week, took: performance.now() - t0 };
}
