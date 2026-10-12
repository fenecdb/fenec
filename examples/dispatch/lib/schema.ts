// Plover's collections, declared in code the way Drizzle declares tables.
// Two kinds of tenant, each a file of its own on a node behind fenec-shard:
//
// - `plover`, the platform's: who may sign in, and to which company;
// - `co-<slug>`, a delivery company's: its couriers, where they are, and the
//   orders they carry. A company's dispatchers, couriers and customers reach
//   this tenant alone (the token's `tenant` claim), and inside it only what
//   policy.txt grants their role.
//
// scripts/setup.ts hands each tenant its schema as FenecQL (`toFenecQL`)
// when the router places it; the app's pages connect with `schema`, which
// checks the tenant holds what the code reads.
import {
  fenecTable,
  text,
  integer,
  doublePrecision,
  timestamp,
  geo,
  index,
  uniqueIndex,
} from '@fenecdb/web/schema';

// ---------------------------------------------------------------- platform

/** The companies on the platform, and the plan each pays for. */
export const companies = fenecTable('companies', {
  slug: text().notNull(),
  name: text().notNull(),
  plan: text().notNull(), // starter | pro (lib/plans.ts)
  created: timestamp(),
}, (t) => [uniqueIndex('companies_slug').on(t.slug)]);

/**
 * Every account, a dispatcher's, a courier's or a customer's. One email is
 * one account whatever its case: `nocase` compares the case fold, so
 * `Ada@Tandem.example` signs in as `ada@tandem.example` and a second sign-up
 * with either is a clash -- kept as it was typed. A staff code is unique in
 * its company, `@unique(company, code)`: two companies may both have a C-001.
 */
export const users = fenecTable('users', {
  uid: text().notNull(), // the token's `sub`
  email: text().notNull().nocase(),
  name: text().notNull(),
  password: text().notNull(), // scrypt: salt and hash
  company: text().notNull(), // the company's slug
  role: text().notNull(), // dispatcher | courier | customer
  code: text(), // staff code: D-001, C-014; none for a customer
  created: timestamp(),
}, (t) => [
  uniqueIndex('users_uid').on(t.uid),
  uniqueIndex('users_email').on(t.email),
  uniqueIndex('users_company_code').on(t.company, t.code),
]);

export const platform = { companies, users };

// ----------------------------------------------------------------- company

/** A company's couriers: who they are, and what they ride. */
export const couriers = fenecTable('couriers', {
  user: text().notNull(), // the account's id in `plover`, the token's `sub`
  code: text().notNull(),
  name: text().notNull(),
  vehicle: text().notNull(), // bike | scooter | car
}, (t) => [
  uniqueIndex('couriers_user').on(t.user),
  uniqueIndex('couriers_code').on(t.code),
]);

/**
 * Where each courier is: written over every few seconds as they move, and
 * gone from every read two minutes after the last -- a phone switched off
 * leaves no courier on the map. `state` is what they are doing: `idle`
 * (waiting for an order), `pickup` (on the way to the merchant), `dropoff`
 * (carrying it).
 */
export const positions = fenecTable('positions', {
  courier: text().notNull(),
  loc: geo().notNull(),
  seen: timestamp().notNull(),
  state: text().notNull(),
  heading: doublePrecision(),
  job: integer(), // the order carried, its id
  vehicle: text(),
}, (t) => [
  uniqueIndex('positions_courier').on(t.courier),
  index('positions_loc').using('gist', t.loc),
  index('positions_seen').on(t.seen).ttl('2m'),
]);

/** Where orders are picked up: restaurants, shops, pharmacies. */
export const merchants = fenecTable('merchants', {
  code: text().notNull(),
  name: text().notNull(),
  kind: text().notNull(),
  address: text().notNull(),
  loc: geo().notNull(),
}, (t) => [
  uniqueIndex('merchants_code').on(t.code),
  // "Order from near you": a customer's nearest merchants, `near loc`.
  index('merchants_loc').using('gist', t.loc),
]);

/**
 * An order is a job in a queue. `run_at` is when it may next be claimed:
 * when it was placed, and while a courier holds it, the end of the lease
 * the courier keeps extending as it moves. A courier who stops -- a phone
 * dead, an app killed -- holds it until the lease lapses; then the next
 * claim takes it. `null` is an order no claim takes: delivered, cancelled,
 * or dead after too many attempts.
 *
 * `claim` names one claim of the order, so a courier whose lease lapsed
 * and whose order went to another finds out at its next write (`require 1`
 * over `claim = $1`). `courier_loc` is the carrying courier's last point,
 * written with each lease extension: the customer follows it on their own
 * order, since a rule cannot say "the position of my order's courier".
 */
export const orders = fenecTable('orders', {
  ref: text().notNull(), // the merchant's own reference
  merchant: text().notNull(),
  customer: text().notNull(), // the customer's account id, their token's `sub`
  pickup: geo().notNull(),
  pickup_name: text().notNull(),
  dropoff: geo().notNull(),
  dropoff_addr: text().notNull(),
  items: integer().notNull(),
  fee: integer().notNull(), // cents
  status: text().notNull(), // ready | assigned | picked | delivered | cancelled | dead
  run_at: timestamp(),
  courier: text(),
  claim: text(),
  attempts: integer().notNull(),
  created: timestamp().notNull(),
  promised: timestamp().notNull(), // delivered by then is on time
  picked_at: timestamp(),
  delivered_at: timestamp(),
  driven: doublePrecision(), // metres the courier drove for it
  courier_loc: geo(),
  error: text(),
}, (t) => [
  // A merchant's reference is unique within the company, so an order
  // sent twice -- a retry after a timeout -- is one order.
  uniqueIndex('orders_merchant_ref').on(t.merchant, t.ref),
  // A courier's latest deliveries: a run of the index, read to the page.
  index('orders_courier_delivered').on(t.courier, t.delivered_at.desc()),
  index('orders_run_at').on(t.run_at),
  // The dashboards' ranges: the last two days, the last fortnight.
  index('orders_delivered').on(t.delivered_at),
  index('orders_pickup').using('gist', t.pickup),
  index('orders_customer').using('hash', t.customer),
  index('orders_status').using('hash', t.status),
  index('orders_claim').using('hash', t.claim),
]);

export const company = { couriers, positions, merchants, orders };

export type Order = typeof orders.$inferSelect;
export type Position = typeof positions.$inferSelect;
export type Courier = typeof couriers.$inferSelect;
export type Merchant = typeof merchants.$inferSelect;
export type User = typeof users.$inferSelect;
