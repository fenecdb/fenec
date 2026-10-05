// Trellis put together: the signing key, the service over the router, and
// its HTTP server. `main.ts` runs it; the tests start it in their process.
import { type Server } from 'node:http';
import { type Config } from './config.ts';
import { loadOrMakeKey } from './jwt.ts';
import { trellisServer } from './server.ts';
import { Trellis } from './trellis.ts';

export async function startApp(cfg: Config, port: number): Promise<{ trellis: Trellis; server: Server; url: string }> {
  const { signing, jwks } = loadOrMakeKey(cfg.keysDir, 'app', 'trellis-app-1');
  const trellis = new Trellis(cfg, signing, jwks.keys);
  // The router may still be starting: try for a while before giving up.
  for (let i = 0; ; i++) {
    try {
      await trellis.provision();
      break;
    } catch (e) {
      if (i > 100) throw e;
      await new Promise((r) => setTimeout(r, 100));
    }
  }
  const server = trellisServer(trellis);
  server.keepAliveTimeout = 30_000;
  await new Promise<void>((r) => server.listen(port, '127.0.0.1', r));
  const addr = server.address() as { port: number };
  return { trellis, server, url: `http://127.0.0.1:${addr.port}` };
}
