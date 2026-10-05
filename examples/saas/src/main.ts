// `npm start`: Trellis on its origin's port, against the router of
// `npm run cluster`, with the mock identity provider beside it unless
// TRELLIS_IDP=0.
import { startApp } from './app.ts';
import { config } from './config.ts';
import { idpServer } from './idp.ts';

const cfg = config();
const { url } = await startApp(cfg, Number(new URL(cfg.origin).port || 3000));
console.log(`Trellis on ${url}`);
if (process.env.TRELLIS_IDP !== '0') {
  const idp = idpServer({
    issuer: cfg.oidcIssuer,
    keysDir: cfg.keysDir,
    clients: { [cfg.oidcClientId]: `${cfg.origin}/api/auth/sso/callback` },
  });
  idp.listen(Number(new URL(cfg.oidcIssuer).port), '127.0.0.1');
  console.log(`mock identity provider on ${cfg.oidcIssuer}`);
}
