// `npm run dev`: the whole product on this machine -- three tenant nodes
// and the router over data/dev, Trellis on :3000, the mock identity
// provider on :3001 -- with the demo organisation seeded the first time.
// Ctrl-C stops all of it; the data stays for the next run.
import { fileURLToPath } from 'node:url';
import { startApp } from '../src/app.ts';
import { Cluster } from '../src/cluster.ts';
import { config } from '../src/config.ts';
import { idpServer } from '../src/idp.ts';
import { DEMO_PASSWORD, seed } from './seed.ts';

const cfg = config({ keysDir: fileURLToPath(new URL('../data/dev/keys', import.meta.url)) });
const cluster = new Cluster({
  dir: fileURLToPath(new URL('../data/dev', import.meta.url)),
  basePort: Number(new URL(cfg.routerUrl).port),
  lease: Number(process.env.TRELLIS_LEASE ?? 5),
  keysDir: cfg.keysDir,
  operatorToken: cfg.operatorToken,
  shardToken: cfg.shardToken,
  nodeFlags: ['--http-max-streams', '512'],
});
await cluster.start();
const { trellis, url } = await startApp(cfg, Number(new URL(cfg.origin).port));
const idp = idpServer({ issuer: cfg.oidcIssuer, keysDir: cfg.keysDir, clients: { [cfg.oidcClientId]: `${cfg.origin}/api/auth/sso/callback` } });
idp.listen(Number(new URL(cfg.oidcIssuer).port), '127.0.0.1');
await seed(url, trellis);
console.log(`Trellis on ${url}  (sign in as ines@lumen.studio, "${DEMO_PASSWORD}"; mail at ${url}/dev/mail)`);
console.log(`router ${cluster.routerUrl}, nodes ${cluster.nodeNames.map((n) => cluster.nodeUrl(n)).join(', ')}, identity provider ${cfg.oidcIssuer}`);
const stop = async () => {
  await cluster.stop();
  process.exit(0);
};
process.on('SIGINT', stop);
process.on('SIGTERM', stop);
