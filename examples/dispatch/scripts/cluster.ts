// `npm run cluster`: two tenant nodes and the router over data/dev, for
// development, until Ctrl-C. `npm run setup` then places the tenants and
// fills them; `npm run simulate` sets the city moving.
import { join } from 'node:path';
import { Cluster } from '../lib/cluster';
import { DATA_DIR, OPERATOR_TOKEN, SHARD_TOKEN, SITE_ORIGIN } from '../lib/config';

const base = Number(process.env.PLOVER_ROUTER_PORT ?? 8090);
const cluster = new Cluster({
  dir: join(DATA_DIR, 'dev'),
  basePort: base,
  nodes: 2,
  cors: SITE_ORIGIN,
  operatorToken: OPERATOR_TOKEN,
  shardToken: SHARD_TOKEN,
});
await cluster.start();
console.log(`router ${cluster.routerUrl}, nodes ${cluster.nodeNames.map((n) => cluster.nodeUrl(n)).join(', ')}`);
const stop = async () => {
  await cluster.stop();
  process.exit(0);
};
process.on('SIGINT', stop);
process.on('SIGTERM', stop);
