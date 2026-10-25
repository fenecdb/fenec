// A stand-in for an identity provider, to try fenec-server's token checks
// with nothing to sign up for. It does what a provider does and no more: it
// holds an RSA key, publishes the public half as a JWKS (`jwks.json`), and
// signs RS256 tokens that name their key in `kid`. The claims of each
// provider's token (`shapes`) are the shapes its documentation describes,
// with made-up values; the signature is this script's, so fenec-server
// accepts them only from a JWKS file this script wrote.
//
//   node idp.mjs init <dir>                  a key, and the JWKS holding it
//   node idp.mjs add-key <dir> <kid>         a second key joins the JWKS and
//                                            signs from now on (a rotation)
//   node idp.mjs token <dir> <shape> <sub> [claims-json]
//                                            clerk | auth0 | supabase | cognito
//                                            | access | plain
//   node idp.mjs ec-token <sub>              an ES256 token, which a server
//                                            that takes RS256 and HS256 refuses
//   node idp.mjs serve <dir>                 the JWKS over HTTP at
//                                            /.well-known/jwks.json, as a
//                                            provider serves it; says where
import { createServer } from 'node:http';
import { createPrivateKey, createSign, generateKeyPairSync, sign as signRaw } from 'node:crypto';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const b64 = (v) => Buffer.from(typeof v === 'string' ? v : JSON.stringify(v)).toString('base64url');

/** Make the first key and write the JWKS that publishes it. */
export function init(dir, kid = 'idp-1') {
  addKey(dir, kid, []);
}

/** A new key joins the JWKS (the old ones stay) and signs from now on. */
export function addKey(dir, kid, keep = readJwks(dir).keys) {
  const { publicKey, privateKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
  writeFileSync(join(dir, `${kid}.pem`), privateKey.export({ type: 'pkcs8', format: 'pem' }));
  writeFileSync(join(dir, 'signing-kid'), kid);
  const jwk = { ...publicKey.export({ format: 'jwk' }), kid, alg: 'RS256', use: 'sig' };
  writeFileSync(join(dir, 'jwks.json'), JSON.stringify({ keys: [...keep, jwk] }, null, 2));
}

function readJwks(dir) {
  return existsSync(join(dir, 'jwks.json')) ? JSON.parse(readFileSync(join(dir, 'jwks.json'), 'utf8')) : { keys: [] };
}

/** An RS256 token for `claims`, signed with the newest key, an hour long. */
export function token(dir, claims, { ttl = 3600 } = {}) {
  const kid = readFileSync(join(dir, 'signing-kid'), 'utf8').trim();
  const key = createPrivateKey(readFileSync(join(dir, `${kid}.pem`)));
  const now = Math.floor(Date.now() / 1000);
  const head = b64({ alg: 'RS256', typ: 'JWT', kid });
  const body = b64({ iat: now, nbf: now, exp: now + ttl, ...claims });
  const sig = createSign('RSA-SHA256').update(`${head}.${body}`).sign(key).toString('base64url');
  return `${head}.${body}.${sig}`;
}

/** The claims of each provider's token, as its documentation lists them
 * (docs/auth lists the pages), with invented values. `sub` is the user's. */
export const shapes = {
  // Clerk's session token, version 2: `o` is the active organization.
  // `role` and `tenant` are not Clerk's: they are what a customized session
  // token adds, and the only way a policy can see them (`o.rol` is nested).
  clerk: (sub) => ({
    iss: 'https://clerk.example.test', sub, azp: 'https://app.example.test', sid: 'sess_2xYz', v: 2,
    o: { id: 'org_2acme', slg: 'acme', rol: 'admin' },
    role: 'member', tenant: 'acme',
  }),
  // Auth0's access token for an API. Its custom claims must be namespaced
  // URLs, which a policy cannot name; `permissions` is Auth0's own, a list.
  auth0: (sub) => ({
    iss: 'https://acme.us.auth0.com/', sub: `auth0|${sub}`, aud: ['https://api.example.test'],
    azp: 'Zr8Example', scope: 'openid profile email', org_id: 'org_acme', // org_id: a login to an Organization
    permissions: ['read:notes'],
    'https://example.test/role': 'member',
  }),
  // Supabase Auth's access token: `role` is the Postgres role, `authenticated`
  // for every signed-in user; `app_metadata` is nested.
  supabase: (sub) => ({
    iss: 'https://abcdefgh.supabase.co/auth/v1', aud: 'authenticated', sub,
    role: 'authenticated', aal: 'aal1', session_id: '5b0f6a3e', email: `${sub}@example.test`,
    is_anonymous: false, app_metadata: { provider: 'email', providers: ['email'] }, user_metadata: {},
  }),
  // Cognito's access token. `cognito:groups` is Cognito's and cannot be named
  // in a policy; `role` is what a pre token generation trigger adds.
  cognito: (sub) => ({
    iss: 'https://cognito-idp.eu-west-1.amazonaws.com/eu-west-1_Example', sub,
    client_id: '1example23456789', token_use: 'access', scope: 'openid', username: sub,
    'cognito:groups': ['editors'], role: 'member',
  }),
  // Cloudflare Access's application token: `aud` is the application's tag,
  // a list, and a team's other applications are signed with the same keys.
  access: (sub) => ({
    iss: 'https://acme.cloudflareaccess.com', sub, email: `${sub}@example.test`,
    aud: ['32eafc7626e974616deaf0dc3ba5c0a0eb5f4d2d1e3b8f0a6c1a2e7d9a4b5c6d'],
  }),
  plain: (sub) => ({ sub }),
};

/** An ES256 token signed by a key no JWKS here holds: what a provider that
 * signs with EC keys sends, which this server does not take. */
export function ecToken(sub) {
  const { privateKey } = generateKeyPairSync('ec', { namedCurve: 'P-256' });
  const now = Math.floor(Date.now() / 1000);
  const head = b64({ alg: 'ES256', typ: 'JWT', kid: 'ec-1' });
  const body = b64({ sub, iat: now, exp: now + 3600 });
  const sig = signRaw('sha256', Buffer.from(`${head}.${body}`), { key: privateKey, dsaEncoding: 'ieee-p1363' });
  return `${head}.${body}.${sig.toString('base64url')}`;
}

/** The JWKS at /.well-known/jwks.json on a port the system picks. */
export function serve(dir) {
  const server = createServer((req, res) => {
    if (req.url !== '/.well-known/jwks.json') return void res.writeHead(404).end('not here');
    res.writeHead(200, { 'content-type': 'application/json' }).end(readFileSync(join(dir, 'jwks.json')));
  });
  server.listen(0, '127.0.0.1', () => console.log(`http://127.0.0.1:${server.address().port}`));
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const [cmd, a, b, c, d] = process.argv.slice(2);
  if (cmd === 'init') init(a);
  else if (cmd === 'add-key') addKey(a, b);
  else if (cmd === 'token') {
    if (!shapes[b]) throw new Error(`shape: ${Object.keys(shapes).join(' | ')}`);
    console.log(token(a, { ...shapes[b](c), ...(d ? JSON.parse(d) : {}) }));
  } else if (cmd === 'ec-token') console.log(ecToken(a));
  else if (cmd === 'serve') serve(a);
  else throw new Error('usage: node idp.mjs init|add-key|token|ec-token|serve ...');
}
