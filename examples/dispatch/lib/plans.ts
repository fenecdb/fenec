// What a company's plan buys: its tenant's quota on the node, which the
// router keeps in its directory and sends wherever the tenant goes
// (`PUT /_shard/tenants/<t>` with `quota`, and `/quota` after).
//
// The rates are what a fleet costs. A courier's phone sends one write every
// three seconds while it moves -- its position, and the lease of the order it
// carries, as one /batch -- about 0.7 KB landed: a fleet of 40 is 13 writes
// and 9 KB a second, and every claim, pickup and delivery adds one. Streams
// are the dispatchers' live views and every courier holding a claim open at
// the server while it waits for an order (`Fenec-Wait`): an idle courier is a
// stream, so the plan bounds how many may wait at once.

export type Plan = 'starter' | 'pro';

export interface Quota {
  /** The tenant's file, bytes. */
  bytes: number;
  /** Rows of its collections together. */
  rows: number;
  /** Subscriptions, held claims and waits at once. */
  streams: number;
  /** Requests in flight at once. */
  requests: number;
  /** Writes a second. */
  writes: number;
  /** Bytes written a second. */
  write_bytes: number;
}

export const PLANS: Record<Plan, { label: string; couriers: number; quota: Quota }> = {
  starter: {
    label: 'Starter',
    couriers: 25,
    quota: {
      bytes: 256 * 2 ** 20,
      rows: 250_000,
      streams: 40,
      requests: 48,
      writes: 40,
      write_bytes: 64 * 2 ** 10,
    },
  },
  pro: {
    label: 'Pro',
    couriers: 250,
    quota: {
      bytes: 4 * 2 ** 30,
      rows: 5_000_000,
      streams: 400,
      requests: 400,
      writes: 600,
      write_bytes: 2 * 2 ** 20,
    },
  },
};
