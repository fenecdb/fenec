// `npm run setup`: the platform's tenant and a tenant each company, through
// the router of `npm run cluster`, filled with the companies' people,
// couriers, merchants and 14 days of past deliveries.
import { ROUTER_URL, SHARD_TOKEN } from '../lib/config';
import { routeTo } from '../lib/db';
import { setup } from '../lib/setup';

const t0 = performance.now();
routeTo(ROUTER_URL);
await setup({
  router: ROUTER_URL,
  shardToken: SHARD_TOKEN,
  historyDays: Number(process.env.PLOVER_HISTORY_DAYS ?? 14),
  log: (s) => console.log(s),
});
console.log(`set up in ${((performance.now() - t0) / 1000).toFixed(1)} s`);
