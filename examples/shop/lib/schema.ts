// The shop's collections, declared in code the way Drizzle declares tables.
// `connect(url, { schema, migrate: true })` makes what is missing on the
// server and refuses anything that would lose data (scripts/seed.ts); the
// pages connect with `schema` alone, which only checks.
import {
  fenecTable,
  text,
  integer,
  doublePrecision,
  timestamp,
  json,
  vector,
  index,
  uniqueIndex,
} from '@fenecdb/web/schema';

/** How many dimensions `products.features` has: see lib/features.ts. */
export const FEATURE_DIMENSIONS = 48;

export const products = fenecTable('products', {
  sku: text().notNull(),
  slug: text().notNull(),
  name: text().notNull(),
  description: text().notNull(),
  category: text().notNull(),
  brand: text().notNull(),
  // Prices are integer minor units (cents): no float ever holds money.
  price: integer().notNull(),
  priceBand: text().notNull(),
  colour: text(),
  material: text(),
  size: text(),
  weight: integer(), // grams
  rating: doublePrecision(),
  reviews: integer(),
  added: timestamp(),
  // The product's attributes as a vector: category, colour, material,
  // price and weight, so `near` finds what is like it (lib/features.ts).
  features: vector({ dimensions: FEATURE_DIMENSIONS }),
}, (t) => [
  uniqueIndex('products_sku').on(t.sku),
  uniqueIndex('products_slug').on(t.slug),
  // Prefixes make a word typed in part, or with its last letters wrong,
  // still find the product: "titan" finds titanium, "harmatan" Harmattan.
  index('products_name').using('bm25', t.name).with({ prefix: 5 }),
  index('products_description').using('bm25', t.description).with({ prefix: 5 }),
  index('products_category').using('hash', t.category),
  index('products_brand').using('hash', t.brand),
  index('products_band').using('hash', t.priceBand),
  index('products_colour').using('hash', t.colour),
  index('products_price').on(t.price),
  index('products_rating').on(t.rating),
  index('products_added').on(t.added),
  index('products_features').using('hnsw', t.features.op('vector_cosine_ops')),
]);

export const categories = fenecTable('categories', {
  slug: text().notNull(),
  name: text().notNull(),
  department: text().notNull(),
  blurb: text().notNull(),
  position: integer().notNull(),
  shape: text().notNull(),
}, (t) => [
  uniqueIndex('categories_slug').on(t.slug),
  index('categories_position').on(t.position),
]);

/**
 * Stock as a counter: `available` is what can still be sold, `reserved`
 * what orders hold awaiting payment, `sold` what was paid for. A checkout
 * moves units from available to reserved only where enough are available
 * (`... where sku = $1 and available >= $2 require 1`), so the three
 * always add up to what was stocked.
 */
export const inventory = fenecTable('inventory', {
  sku: text().notNull(),
  available: integer().notNull(),
  reserved: integer().notNull(),
  sold: integer().notNull(),
}, (t) => [uniqueIndex('inventory_sku').on(t.sku)]);

/**
 * A cart is its lines. `owner` is the token's subject -- `u:<user>` for a
 * signed-in shopper, `g:<hash of the cookie>` for a guest -- and the policy
 * file holds every scoped token to its own. `line` is owner and sku in one
 * value, since a unique index takes one field. A line not touched for a
 * week expires.
 */
export const carts = fenecTable('carts', {
  owner: text().notNull(),
  line: text().notNull(),
  sku: text().notNull(),
  qty: integer().notNull(),
  touched: timestamp(),
}, (t) => [
  index('carts_owner').using('hash', t.owner),
  uniqueIndex('carts_line').on(t.line),
  index('carts_touched').on(t.touched).ttl('7d'),
]);

export const orders = fenecTable('orders', {
  number: text().notNull(),
  owner: text().notNull(),
  status: text().notNull(), // reserved | paid | cancelled
  subtotal: integer().notNull(),
  shipping: integer().notNull(),
  total: integer().notNull(),
  email: text().notNull(),
  address: json(),
  at: timestamp(),
  holdUntil: timestamp(),
  payment: text(),
}, (t) => [
  uniqueIndex('orders_number').on(t.number),
  index('orders_owner').using('hash', t.owner),
  index('orders_status').using('hash', t.status),
  index('orders_hold').on(t.holdUntil),
]);

export const orderLines = fenecTable('order_lines', {
  orderNumber: text().notNull(),
  owner: text().notNull(),
  sku: text().notNull(),
  name: text().notNull(),
  qty: integer().notNull(),
  price: integer().notNull(),
}, (t) => [
  index('order_lines_order').using('hash', t.orderNumber),
  index('order_lines_owner').using('hash', t.owner),
  index('order_lines_sku').using('hash', t.sku),
]);

/** One row a captured payment: a second capture of an order is a clash. */
export const payments = fenecTable('payments', {
  orderNumber: text().notNull(),
  ref: text().notNull(),
  amount: integer().notNull(),
  at: timestamp(),
}, (t) => [uniqueIndex('payments_order').on(t.orderNumber)]);

export const users = fenecTable('users', {
  email: text().notNull(),
  name: text().notNull(),
  password: text().notNull(), // scrypt, salt and hash
  created: timestamp(),
}, (t) => [uniqueIndex('users_email').on(t.email)]);

export const schema = { products, categories, inventory, carts, orders, order_lines: orderLines, payments, users };

export type Product = typeof products.$inferSelect;
export type Category = typeof categories.$inferSelect;
