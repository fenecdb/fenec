// Seeds fenec-server with the generated catalog: 30 categories, 10 000
// products and their stock. The schema is the code's (lib/schema.ts),
// applied with `migrate: true` -- which takes the server's token -- before
// anything is written. Seeding a database that holds the catalog already
// does nothing; `--force` is for an empty one only.
//
//   npm run seed                 10 000 products
//   SHOP_PRODUCTS=2000 npm run seed
import { connect } from '@fenecdb/web/client';
import { schema, products, categories, inventory } from '../lib/schema';
import { generate } from '../lib/generate';

const url = process.env.FENEC_URL ?? 'http://127.0.0.1:8080';
const token = process.env.FENEC_TOKEN ?? 'shop-dev-token';
const count = Number(process.env.SHOP_PRODUCTS ?? 10_000);

const t0 = performance.now();
const shop = await connect(url, { token, schema, migrate: true });
const held = await shop.from(products).count();
if (held > 0) {
  console.log(`the catalog is there already: ${held} products`);
  process.exit(0);
}

const { products: items, categories: cats } = generate(count);
await shop.from(categories).insert(
  cats.map((c, i) => ({ slug: c.slug, name: c.name, department: c.department, blurb: c.blurb, position: i, shape: c.shape })),
);
const CHUNK = 500;
for (let i = 0; i < items.length; i += CHUNK) {
  const part = items.slice(i, i + CHUNK);
  await shop.from(products).insert(part.map(({ stock: _stock, ...p }) => p));
  await shop.from(inventory).insert(part.map((p) => ({ sku: p.sku, available: p.stock, reserved: 0, sold: 0 })));
}
const ms = performance.now() - t0;
console.log(`seeded ${cats.length} categories and ${items.length} products in ${(ms / 1000).toFixed(1)} s`);
