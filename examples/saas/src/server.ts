// Trellis's HTTP server: the auth API, the organisation API, the page
// shell and its scripts, and /db/, a pipe to the router for the browser.
//
// A plain node:http server. Its job is small on purpose -- it holds the
// signing key and the credentials, and everything else goes to fenecdb
// with a scoped token -- so a framework would add a runtime, a build and a
// second place where requests are routed, and nothing this needs.
//
// The browser reads and writes its organisation through /db/ with its own
// access token: this server passes the request to fenec-shard as it came
// and the node holds it to policy.txt. A page on another origin could reach
// the router itself now -- fenec-server answers a CORS preflight before it
// asks for a token, and the nodes allow this origin -- but /db/ stays: one
// origin behind one TLS terminator is how this would be deployed, the CSP's
// `connect-src 'self'` holds the page to it, and the router stays off the
// public network. Behind the pipe the router counts refusals by this
// server's address, so every refusal through the pipe shares one doubling
// wait; a page's tokens are minted here and refreshed half a minute before
// they lapse, so a person's own requests are seldom among them, and a
// deployment that wants the router's per-client waits points the page at
// the router (README, gaps).
import { createReadStream, existsSync, statSync } from 'node:fs';
import { createServer, request as httpRequest, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { JwtError } from './jwt.ts';
import { Oidc } from './oidc.ts';
import { Trellis, type Caller, type Reply } from './trellis.ts';
import { shell, mailPage } from './views.ts';

const RT_COOKIE = 'trellis_rt';
const SSO_COOKIE = 'trellis_sso';
const PUBLIC = fileURLToPath(new URL('../public/', import.meta.url));
const FENEC_WEB = dirname(createRequire(import.meta.url).resolve('@fenecdb/web/client'));
const VENDOR: Record<string, string> = {
  'client.js': join(FENEC_WEB, 'client.js'),
  'builder.js': join(FENEC_WEB, 'builder.js'),
  'http.js': join(FENEC_WEB, 'http.js'),
};

/** The headers a request to the database may carry through /db/, and an answer back. */
const PASS_UP = ['authorization', 'content-type', 'accept', 'idempotency-key', 'if-none-match', 'fenec-after', 'fenec-wait'];
const PASS_DOWN = ['content-type', 'etag', 'fenec-seq', 'fenec-next', 'retry-after', 'idempotent-replayed', 'cache-control'];

const PAGE_ROUTES = /^\/(?:$|signin$|signup$|forgot$|reset$|verify$|account$|orgs\/new$|invite\/[^/]+\/[^/]+$|o\/[a-z0-9-]+(?:\/(?:board|members|search|audit)(?:\/[^/]*)?)?$)/;

export function trellisServer(trellis: Trellis): Server {
  const cfg = trellis.cfg;
  const oidc = new Oidc(cfg.oidcIssuer, cfg.oidcClientId, `${cfg.origin}/api/auth/sso/callback`);
  const secure = cfg.insecureCookies ? '' : '; Secure';
  const router = new URL(cfg.routerUrl);

  const ipOf = (req: IncomingMessage): string => {
    const fwd = cfg.trustProxy ? String(req.headers['x-forwarded-for'] ?? '').split(',')[0].trim() : '';
    return fwd || req.socket.remoteAddress || 'unknown';
  };

  function setRefresh(res: ServerResponse, value: string | null | undefined) {
    if (value === undefined) return;
    res.appendHeader(
      'set-cookie',
      value === null
        ? `${RT_COOKIE}=; Path=/api/auth; HttpOnly; SameSite=Strict; Max-Age=0${secure}`
        : `${RT_COOKIE}=${value}; Path=/api/auth; HttpOnly; SameSite=Strict; Max-Age=${14 * 86400}${secure}`,
    );
  }

  function answer(res: ServerResponse, r: Reply) {
    setRefresh(res, r.refresh);
    json(res, r.status, r.body);
  }

  async function handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
    const url = new URL(req.url ?? '/', 'http://x');
    const path = url.pathname;
    const method = req.method ?? 'GET';
    securityHeaders(res);

    if (path.startsWith('/db/')) return proxy(req, res, url);

    if (method === 'GET' && path === '/healthz') return json(res, 200, { ok: true });
    if (method === 'GET' && path.startsWith('/static/')) return file(res, join(PUBLIC, path.slice('/static/'.length)));
    if (method === 'GET' && path.startsWith('/vendor/fenec/')) {
      const f = VENDOR[path.slice('/vendor/fenec/'.length)];
      return f ? file(res, f) : json(res, 404, { error: 'Not found.' });
    }
    if (method === 'GET' && PAGE_ROUTES.test(path)) return html(res, 200, shell());
    if (method === 'GET' && path === '/dev/mail' && cfg.devMail) {
      return html(res, 200, mailPage(await trellis.outbox(url.searchParams.get('to') ?? undefined)));
    }

    // ---- SSO: a top-level navigation, so a GET and a Lax cookie.
    if (method === 'GET' && path === '/api/auth/sso') {
      const s = oidc.start();
      res.appendHeader('set-cookie', `${SSO_COOKIE}=${s.state}; Path=/api/auth/sso; HttpOnly; SameSite=Lax; Max-Age=600${secure}`);
      return redirect(res, s.location);
    }
    if (method === 'GET' && path === '/api/auth/sso/callback') {
      res.appendHeader('set-cookie', `${SSO_COOKIE}=; Path=/api/auth/sso; HttpOnly; SameSite=Lax; Max-Age=0${secure}`);
      try {
        const id = await oidc.finish(url.searchParams.get('code') ?? '', url.searchParams.get('state') ?? '', cookies(req)[SSO_COOKIE]);
        const r = await trellis.signInWithIdentity({ issuer: cfg.oidcIssuer, ...id }, ipOf(req), String(req.headers['user-agent'] ?? ''));
        setRefresh(res, r.refresh);
        return redirect(res, r.status === 200 ? '/' : `/signin?error=${encodeURIComponent(String(r.body.error))}`);
      } catch (e) {
        if (!(e instanceof JwtError)) throw e;
        return redirect(res, `/signin?error=${encodeURIComponent('Single sign-on did not complete. Try again.')}`);
      }
    }

    if (!path.startsWith('/api/')) return json(res, 404, { error: 'Not found.' });

    // A write from another origin is refused, and the API takes JSON
    // alone, which a cross-site form cannot send; the refresh cookie is
    // SameSite=Strict besides.
    if (method !== 'GET') {
      const origin = req.headers.origin;
      if (origin && origin !== cfg.origin) return json(res, 403, { error: 'Cross-origin request refused.' });
      if (method !== 'DELETE' && !String(req.headers['content-type'] ?? '').startsWith('application/json')) {
        return json(res, 415, { error: 'The API takes application/json.' });
      }
    }
    const b = method === 'GET' ? {} : await body(req);
    const ip = ipOf(req);
    const agent = String(req.headers['user-agent'] ?? '');
    const rt = cookies(req)[RT_COOKIE];

    if (method === 'POST') {
      switch (path) {
        case '/api/auth/signup':
          return answer(res, await trellis.signUp({ email: str(b.email), name: str(b.name), password: str(b.password) }, ip));
        case '/api/auth/signin':
          return answer(res, await trellis.signIn({ email: str(b.email), password: str(b.password) }, ip, agent));
        case '/api/auth/refresh':
          return answer(res, await trellis.refresh(rt, ip, agent, b.org === undefined ? undefined : str(b.org)));
        case '/api/auth/signout':
          return answer(res, await trellis.signOut(rt, ip));
        case '/api/auth/verify':
          return answer(res, await trellis.verifyEmail(str(b.token), ip));
        case '/api/auth/forgot':
          return answer(res, await trellis.forgot(str(b.email), ip));
        case '/api/auth/reset':
          return answer(res, await trellis.reset(str(b.token), str(b.password), ip));
      }
    }

    // ---- Signed in, no organisation: the API token.
    if (path === '/api/orgs' || path === '/api/invites/accept' || path === '/api/me/security') {
      const c = trellis.caller(req.headers.authorization);
      if (!c) return json(res, 401, { error: 'Sign in to continue.' });
      if (method === 'POST' && path === '/api/orgs') return answer(res, await trellis.createOrg(c, { name: str(b.name), slug: str(b.slug) }));
      if (method === 'POST' && path === '/api/invites/accept') return answer(res, await trellis.acceptInvite(c, str(b.org), str(b.code)));
      if (method === 'GET' && path === '/api/me/security') return json(res, 200, { events: await trellis.securityLog(c.sub) });
      return json(res, 405, { error: 'Method not allowed.' });
    }

    // ---- Inside an organisation: the person's own token for it, which
    // the database holds to their role.
    const m = /^\/api\/orgs\/([a-z0-9-]+)\/(invites|teams|members\/([A-Za-z0-9_]+))$/.exec(path);
    if (m) {
      const c: Caller | null = trellis.caller(req.headers.authorization, m[1]);
      if (!c) return json(res, 401, { error: 'Sign in to continue.' });
      if (method === 'POST' && m[2] === 'invites') {
        return answer(res, await trellis.invite(c, { email: str(b.email), role: str(b.role), teams: strs(b.teams) }));
      }
      if (method === 'POST' && m[2] === 'teams') return answer(res, await trellis.createTeam(c, str(b.name)));
      if (m[3] && method === 'PATCH') {
        return answer(res, await trellis.updateMember(c, m[3], { role: b.role === undefined ? undefined : str(b.role), teams: b.teams === undefined ? undefined : strs(b.teams) }));
      }
      if (m[3] && method === 'DELETE') return answer(res, await trellis.removeMember(c, m[3]));
      return json(res, 405, { error: 'Method not allowed.' });
    }
    return json(res, 404, { error: 'Not found.' });
  }

  /**
   * /db/t/<tenant>/... to the router, as it came: an organisation's tenant
   * only (never the accounts tenant, whose rules no person's token meets
   * anyway), the request's own Authorization, the answer streamed back --
   * a subscription included.
   */
  function proxy(req: IncomingMessage, res: ServerResponse, url: URL) {
    const m = /^\/db\/t\/(o-[a-z0-9-]{3,40})(\/[A-Za-z0-9_/]*)$/.exec(url.pathname);
    if (!m || !['GET', 'POST', 'PATCH', 'DELETE', 'HEAD'].includes(req.method ?? '')) {
      return json(res, 404, { error: 'Not found.' });
    }
    const headers: Record<string, string> = {};
    for (const h of PASS_UP) {
      const v = req.headers[h];
      if (typeof v === 'string') headers[h] = v;
    }
    if (req.headers['content-length']) headers['content-length'] = String(req.headers['content-length']);
    const up = httpRequest(
      {
        host: router.hostname,
        port: router.port,
        method: req.method,
        path: `/t/${m[1]}${m[2]}${url.search}`,
        headers,
      },
      (r) => {
        const out: Record<string, string> = {};
        for (const h of PASS_DOWN) {
          const v = r.headers[h];
          if (typeof v === 'string') out[h] = v;
        }
        res.writeHead(r.statusCode ?? 502, out);
        if (out['content-type']?.startsWith('text/event-stream')) res.flushHeaders();
        r.pipe(res);
      },
    );
    up.on('error', () => {
      if (!res.headersSent) json(res, 502, { error: 'The database could not be reached. Try again.' });
      else res.destroy();
    });
    res.on('close', () => up.destroy());
    req.pipe(up);
  }

  return createServer((req, res) => {
    handle(req, res).catch((e) => {
      console.error(e);
      if (!res.headersSent) json(res, 500, { error: 'Something went wrong on our side. Try again.' });
      else res.destroy();
    });
  });
}

// ------------------------------------------------------------------ helpers

function securityHeaders(res: ServerResponse) {
  res.setHeader(
    'content-security-policy',
    "default-src 'self'; script-src 'self'; style-src 'self' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; " +
      "img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'",
  );
  res.setHeader('x-content-type-options', 'nosniff');
  res.setHeader('referrer-policy', 'no-referrer');
  res.setHeader('cross-origin-opener-policy', 'same-origin');
}

function json(res: ServerResponse, status: number, body: unknown) {
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store' });
  res.end(JSON.stringify(body));
}

function html(res: ServerResponse, status: number, body: string) {
  res.writeHead(status, { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' });
  res.end(body);
}

function redirect(res: ServerResponse, to: string) {
  res.writeHead(302, { location: to, 'cache-control': 'no-store' });
  res.end();
}

const TYPES: Record<string, string> = { js: 'text/javascript; charset=utf-8', css: 'text/css; charset=utf-8', svg: 'image/svg+xml' };

function file(res: ServerResponse, path: string) {
  if (path.includes('..') || !existsSync(path) || !statSync(path).isFile()) return json(res, 404, { error: 'Not found.' });
  res.writeHead(200, { 'content-type': TYPES[path.split('.').pop() ?? ''] ?? 'application/octet-stream', 'cache-control': 'no-cache' });
  createReadStream(path).pipe(res);
}

function cookies(req: IncomingMessage): Record<string, string> {
  const out: Record<string, string> = {};
  for (const part of String(req.headers.cookie ?? '').split(';')) {
    const i = part.indexOf('=');
    if (i > 0) out[part.slice(0, i).trim()] = part.slice(i + 1).trim();
  }
  return out;
}

async function body(req: IncomingMessage): Promise<Record<string, unknown>> {
  let raw = '';
  for await (const chunk of req) {
    raw += chunk;
    if (raw.length > 64 * 1024) throw new Error('body too large');
  }
  try {
    const v = raw ? JSON.parse(raw) : {};
    return v && typeof v === 'object' && !Array.isArray(v) ? v : {};
  } catch {
    return {};
  }
}

const str = (v: unknown) => (typeof v === 'string' ? v : '');
const strs = (v: unknown) => (Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : []);

