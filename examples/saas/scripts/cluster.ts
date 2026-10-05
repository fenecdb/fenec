// `npm run cluster`: three tenant nodes and the router in the foreground,
// over data/cluster, until Ctrl-C. TRELLIS_LEASE=<s> turns on automatic
// failover (default 5); 0 leaves it to a person.
import { fileURLToPath } from 'node:url';
import { Cluster } from '../src/cluster.ts';
import { config } from '../src/config.ts';

const cfg = config();
const port = Number(new URL(cfg.routerUrl).port);
const cluster = new Cluster({
  dir: fileURLToPath(new URL('../data/cluster', import.meta.url)),
  basePort: port,
  lease: Number(process.env.TRELLIS_LEASE ?? 5),
  cors: cfg.origin,
  keysDir: cfg.keysDir,
  operatorToken: cfg.operatorToken,
  shardToken: cfg.shardToken,
});
await cluster.start();
console.log(`router ${cluster.routerUrl}; nodes ${cluster.nodeNames.map((n) => cluster.nodeUrl(n)).join(', ')}`);
const stop = async () => {
  await cluster.stop();
  process.exit(0);
};
process.on('SIGINT', stop);
process.on('SIGTERM', stop);
