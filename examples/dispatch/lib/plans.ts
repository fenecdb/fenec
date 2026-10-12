// What a company's plan buys: its tenant's quota on the node, which the
// router keeps in its directory and sends wherever the tenant goes
// (`PUT /_shard/tenants/<t>` with `quota`, and `/quota` after).
//
// The rates are what a fleet costs. A courier's phone sends one write every
// three seconds while it moves -- its position, and the lease of the order it
// carries, as one /batch -- two rows and about 0.7 KB landed: a fleet of 40 is
// 27 rows and 9 KB a second, and every claim, pickup and delivery adds a row.
// A write that writes nothing -- a claim that took no order -- costs none.
// Streams are the dispatchers' live views; held claims are the couriers
// waiting at the server for an order (`Fenec-Wait`), each a connection, so
// the plan bounds how many may wait at once apart from how many maps stream.

export type Plan = 'starter' | 'pro';

export interface Quota {
  /** The tenant's file, bytes. */
  bytes: number;
  /** Rows of its collections together. */
  rows: number;
  /** Subscriptions and waits for changes at once: the dispatchers' maps. */
  streams: number;
  /** Claims held at the server at once: couriers waiting for an order. */
  held: number;
  /** Requests in flight at once. */
  requests: number;
  /** Rows written a second. */
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
      held: 40,
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
      held: 400,
      requests: 400,
      writes: 600,
      write_bytes: 2 * 2 ** 20,
    },
  },
};
