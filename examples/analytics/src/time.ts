// Time buckets as FenecQL's `bucket` makes them: UTC, from the epoch, and a
// week from a Monday. The worker keys its rollups by these, and the
// dashboard's raw queries group by `bucket`, so both must agree to the ms
// (test/correctness.test.ts holds them to the server's).
export const MINUTE = 60_000;
export const HOUR = 3_600_000;
export const DAY = 86_400_000;
export const WEEK = 7 * DAY;
/** 1970-01-05, the first Monday after the epoch. */
const MONDAY = 4 * DAY;

export const floor = (ms: number, step: number) => Math.floor(ms / step) * step;
export const minuteOf = (ms: number) => floor(ms, MINUTE);
export const dayOf = (ms: number) => floor(ms, DAY);
export const weekOf = (ms: number) => floor(ms - MONDAY, WEEK) + MONDAY;

export const iso = (ms: number) => new Date(ms).toISOString().replace('.000Z', 'Z');

/** FenecQL's text for a bucket width. */
export function interval(ms: number): string {
  if (ms % WEEK === 0) return `${ms / WEEK}w`;
  if (ms % DAY === 0) return `${ms / DAY}d`;
  if (ms % HOUR === 0) return `${ms / HOUR}h`;
  if (ms % MINUTE === 0) return `${ms / MINUTE}m`;
  return `${ms}ms`;
}
