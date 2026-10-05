// Makes the tenants ready: `npm run setup` after `npm run db`.
//
//   LEDGER_TENANTS   acme,globex   the tenants to make; the first gets the demo people and accounts
import { createTenant, DEMO_PASSWORD, seedDemo } from '../src/setup.ts';

const tenants = (process.env.LEDGER_TENANTS ?? 'acme,globex').split(',').filter(Boolean);
for (const t of tenants) await createTenant(t);
await seedDemo(tenants[0]);
console.log(
  `tenants ${tenants.join(', ')} ready; sign in to ${tenants[0]} as ops (an operator) or ada, ben, cleo, dev, password ${DEMO_PASSWORD}`,
);
