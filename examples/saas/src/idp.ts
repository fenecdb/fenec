// A stand-in identity provider for "Continue with single sign-on": OpenID
// Connect's authorization code flow with PKCE, ID tokens signed RS256 with
// a key of its own and published as a JWKS. It signs in whoever types an
// address -- it is a mock, for development and the tests, in place of
// Okta, Entra ID or Google.
import { createHash, randomBytes } from 'node:crypto';
import { createServer, type Server } from 'node:http';
import { loadOrMakeKey, signJwt } from './jwt.ts';

interface Grant {
  client: string;
  redirect: string;
  nonce: string;
  challenge: string;
  email: string;
  name: string;
  at: number;
}

export function idpServer(opts: { issuer: string; keysDir: string; clients: Record<string, string> }): Server {
  const { signing, jwks } = loadOrMakeKey(opts.keysDir, 'idp', 'idp-1');
  const codes = new Map<string, Grant>();

  return createServer(async (req, res) => {
    const url = new URL(req.url ?? '/', opts.issuer);
    const send = (status: number, type: string, body: string, headers: Record<string, string> = {}) => {
      res.writeHead(status, { 'content-type': type, 'cache-control': 'no-store', ...headers });
      res.end(body);
    };
    const json = (status: number, body: unknown) => send(status, 'application/json', JSON.stringify(body));
    let raw = '';
    for await (const chunk of req) raw += chunk;
    const form = new URLSearchParams(raw);

    if (url.pathname === '/.well-known/openid-configuration') {
      return json(200, {
        issuer: opts.issuer,
        authorization_endpoint: `${opts.issuer}/authorize`,
        token_endpoint: `${opts.issuer}/token`,
        jwks_uri: `${opts.issuer}/jwks`,
        id_token_signing_alg_values_supported: ['RS256'],
        code_challenge_methods_supported: ['S256'],
      });
    }
    if (url.pathname === '/jwks') return json(200, jwks);

    if (url.pathname === '/authorize') {
      const p = req.method === 'POST' ? form : url.searchParams;
      const client = p.get('client_id') ?? '';
      const redirect = p.get('redirect_uri') ?? '';
      if (opts.clients[client] !== redirect) return send(400, 'text/plain', 'unknown client or redirect');
      if (p.get('code_challenge_method') !== 'S256' || !p.get('code_challenge')) return send(400, 'text/plain', 'PKCE (S256) required');
      if (req.method === 'GET') return send(200, 'text/html; charset=utf-8', page(p));
      const email = (p.get('email') ?? '').trim();
      if (!email) return send(400, 'text/plain', 'email required');
      const code = randomBytes(24).toString('base64url');
      codes.set(code, {
        client,
        redirect,
        nonce: p.get('nonce') ?? '',
        challenge: p.get('code_challenge')!,
        email,
        name: (p.get('name') ?? '').trim(),
        at: Date.now(),
      });
      const back = new URL(redirect);
      back.searchParams.set('code', code);
      back.searchParams.set('state', p.get('state') ?? '');
      return send(302, 'text/plain', '', { location: back.toString() });
    }

    if (url.pathname === '/token' && req.method === 'POST') {
      const code = form.get('code') ?? '';
      const g = codes.get(code);
      codes.delete(code);
      if (!g || Date.now() - g.at > 60_000) return json(400, { error: 'invalid_grant' });
      const verifier = form.get('code_verifier') ?? '';
      const challenge = createHash('sha256').update(verifier).digest('base64url');
      if (challenge !== g.challenge || form.get('client_id') !== g.client || form.get('redirect_uri') !== g.redirect) {
        return json(400, { error: 'invalid_grant' });
      }
      const now = Math.floor(Date.now() / 1000);
      const id_token = signJwt(
        {
          iss: opts.issuer,
          aud: g.client,
          sub: createHash('sha256').update(g.email.toLowerCase()).digest('hex').slice(0, 24),
          email: g.email,
          email_verified: true,
          name: g.name,
          nonce: g.nonce,
          iat: now,
          exp: now + 300,
        },
        signing,
      );
      return json(200, { id_token, token_type: 'Bearer', expires_in: 300 });
    }
    send(404, 'text/plain', 'not found');
  });
}

const esc = (s: string) => s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

function page(p: URLSearchParams): string {
  const hidden = ['client_id', 'redirect_uri', 'state', 'nonce', 'code_challenge', 'code_challenge_method', 'response_type', 'scope']
    .map((k) => `<input type="hidden" name="${k}" value="${esc(p.get(k) ?? '')}">`)
    .join('');
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Mock identity provider</title>
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:26rem;margin:4rem auto;padding:0 16px;background:#eef1f4;color:#1b2430}
form{background:#fff;padding:1.5rem;border-radius:8px;border:1px solid #c9d2dc}label{display:block;margin:.8rem 0 .2rem}
input[type=email],input[type=text]{width:100%;padding:.5rem;font:inherit;box-sizing:border-box}button{margin-top:1rem;padding:.5rem 1rem;font:inherit}</style></head>
<body><h1>Mock identity provider</h1><p>This stands in for your company's single sign-on. It signs in any address you type.</p>
<form method="post" action="/authorize">${hidden}
<label for="email">Work email</label><input id="email" name="email" type="email" required autocomplete="email">
<label for="name">Name</label><input id="name" name="name" type="text" autocomplete="name">
<button type="submit">Sign in to Trellis</button></form></body></html>`;
}
