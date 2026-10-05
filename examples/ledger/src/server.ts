// The ledger's server: the operator console and a JSON API, over
// fenec-server. A plain node:http server rendering strings: the console is
// a few tables and forms, so a framework would add its runtime and build to
// every page and nothing the ledger needs.
//
// It holds three kinds of token, none of them the operator's:
//
// - the app's (`role: app`), the only one that moves money. policy.txt lets
//   it update balances and append to the journal, never edit or delete an
//   entry, so this process, compromised, cannot rewrite history;
// - an operator's (`role: admin`), which reads everything and writes
//   nothing: what an operator changes goes through the app's, into `events`;
// - a customer's, whose `accounts` claim lists the accounts they hold,
//   joint ones included: every page a customer sees is read with it, so the
//   database, not this code, decides what they can see.
import { randomUUID } from 'node:crypto';
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { RATE_LIMIT, SESSION_SECRET, SINK_FILE, TENANT, tenantUrl } from './config.ts';
import { Ledger, type Result } from './ledger.ts';
import { parseAmount } from './money.ts';
import { reconcile } from './reconcile.ts';
import { readSink, sinkStatus } from './sink.ts';
import { HttpStore, StoreError, type Store } from './store.ts';
import { sign, Tokens, unsign } from './tokens.ts';
import { holdings, signIn, type User } from './users.ts';
import * as v from './views.ts';

export interface Options {
  tenant?: string;
  rateLimit?: number;
  /** Release lapsed holds every so often, ms; 0 for never. */
  reapEvery?: number;
  sinkFile?: string;
  /** Cookies without `Secure`, for plain http on localhost. */
  insecureCookies?: boolean;
}

interface Ctx {
  user: User;
  /** The store that reads as this person: their own token. */
  reader: Store;
  /** For a customer, the accounts they hold. */
  accounts: string[] | null;
}

const COOKIE = 'quire_session';

export function ledgerServer(opts: Options = {}): Server & { ledger: () => Ledger } {
  const tenant = opts.tenant ?? TENANT;
  const tokens = new Tokens(tenant);
  const base = tenantUrl(tenant);
  const appStore = () => new HttpStore(base, tokens.app());
  const ledger = () => new Ledger(appStore(), { rateLimit: opts.rateLimit ?? RATE_LIMIT });
  const secure = !(opts.insecureCookies ?? process.env.LEDGER_INSECURE_COOKIES === '1');

  async function context(req: IncomingMessage): Promise<Ctx | null> {
    const raw = cookies(req)[COOKIE];
    const value = unsign(raw, SESSION_SECRET);
    if (!value) return null;
    let s: { sub: string; display: string; role: User['role']; exp: number };
    try {
      s = JSON.parse(Buffer.from(value, 'base64url').toString());
    } catch {
      return null;
    }
    if (!s || s.exp < Date.now()) return null;
    const user: User = { name: s.sub, display: s.display, role: s.role };
    if (user.role === 'admin') return { user, reader: new HttpStore(base, tokens.admin(user.name)), accounts: null };
    // Looked up on every request, so an account opened or a holder removed
    // counts at once rather than when a token runs out.
    const accounts = await holdings(appStore(), user.name);
    return { user, reader: new HttpStore(base, tokens.customer(user.name, accounts)), accounts };
  }

  async function handle(req: IncomingMessage, res: ServerResponse): Promise<void> {
    const url = new URL(req.url ?? '/', 'http://x');
    const path = url.pathname;
    const method = req.method ?? 'GET';
    const json = path.startsWith('/api/');

    // A write from a page of another origin is refused: the session cookie
    // is SameSite=Lax as well, and the API wants JSON, which a form cannot send.
    if (method === 'POST') {
      const origin = req.headers.origin;
      if (origin && origin !== `http://${req.headers.host}` && origin !== `https://${req.headers.host}`) {
        return send(res, 403, json ? { error: 'cross-origin request refused' } : 'Cross-origin request refused.');
      }
      if (json && !String(req.headers['content-type'] ?? '').startsWith('application/json')) {
        return send(res, 415, { error: 'the API takes application/json' });
      }
    }

    if (path === '/login' && method === 'GET') return html(res, 200, v.page('Sign in', v.loginPage(), { tenant }));
    if ((path === '/login' || path === '/api/login') && method === 'POST') {
      const b = await body(req);
      const user = await signIn(appStore(), String(b.name ?? ''), String(b.password ?? ''));
      if (!user) {
        return json
          ? send(res, 401, { error: 'wrong name or password' })
          : html(res, 401, v.page('Sign in', v.loginPage('That name and password do not match.'), { tenant }));
      }
      const session = Buffer.from(
        JSON.stringify({ sub: user.name, display: user.display, role: user.role, exp: Date.now() + 8 * 3600_000 }),
      ).toString('base64url');
      res.setHeader(
        'set-cookie',
        `${COOKIE}=${sign(session, SESSION_SECRET)}; Path=/; HttpOnly; SameSite=Lax; Max-Age=28800${secure ? '; Secure' : ''}`,
      );
      return json ? send(res, 200, { user }) : redirect(res, '/accounts');
    }
    if (path === '/logout' && method === 'POST') {
      res.setHeader('set-cookie', `${COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0`);
      return redirect(res, '/login');
    }

    const ctx = await context(req);
    if (!ctx) return json ? send(res, 401, { error: 'sign in first' }) : redirect(res, '/login');
    const admin = ctx.user.role === 'admin';
    const page = (title: string, at: string, inner: string, status = 200) =>
      html(res, status, v.page(title, inner, { user: ctx.user, tenant, at }));
    const q = url.searchParams;

    // ---- pages
    if (method === 'GET' && (path === '/' || path === '/accounts')) {
      const rows = await ctx.reader.rows<v.AccountRow>('get accounts order kind, currency, ext limit 500');
      return page('Accounts', '/accounts', v.accountsPage(ctx.user, rows, q, randomUUID()));
    }
    const acct = /^\/accounts\/([^/]+)$/.exec(path);
    if (method === 'GET' && acct) {
      const ext = decodeURIComponent(acct[1]);
      const [a] = await ctx.reader.rows<v.AccountRow>('get accounts where ext = $1 limit 1', [ext]);
      if (!a) return page('Not found', '', v.notFound(), 404);
      const entries = await ctx.reader.rows<v.EntryRow>('get journal where account = $1 order at desc limit 100', [ext]);
      const holds = await ctx.reader.rows<{ ref: string; amount: number; until: string; state: string }>(
        'get holds where account = $1 and state = "held" order until limit 50',
        [ext],
      );
      return page(a.name, '/accounts', v.accountPage(a, entries, holds));
    }
    if (method === 'GET' && path === '/transfers') {
      const mine = await ctx.reader.rows<v.AccountRow>('get accounts order ext limit 500');
      const rows = await ctx.reader.rows<v.TransferRow>('get transfers order at desc limit 60');
      // An operator may refund anything; a customer what was paid to them.
      const own = new Set(ctx.accounts ?? []);
      const refundable = new Set(rows.filter((t) => admin || own.has(t.dst)).map((t) => t.ref));
      return page('Transfers', '/transfers', v.transfersPage(ctx.user, mine, rows, refundable, q, randomUUID()));
    }
    if (method === 'GET' && path === '/journal') {
      const account = q.get('account') || null;
      const rows = account
        ? await ctx.reader.rows<v.EntryRow>('get journal where account = $1 order at desc limit 200', [account])
        : await ctx.reader.rows<v.EntryRow>('get journal order at desc limit 200');
      return page('Journal', '/journal', v.journalPage(rows, account));
    }
    if (method === 'GET' && path === '/reconciliation') {
      if (!admin) return page('Not found', '', v.notFound(), 404);
      const report = await reconcile(ctx.reader);
      return page(
        'Reconciliation',
        '/reconciliation',
        v.reconciliationPage(report, await sinkState(opts.sinkFile ?? SINK_FILE, report.entries)),
      );
    }

    // ---- actions, from a form (redirect back) or the API (JSON)
    if (method === 'POST') {
      const b = await body(req);
      const key = String(req.headers['idempotency-key'] ?? b.key ?? '');
      const done = (r: Result, back: string, ok: string) => {
        if (json) return send(res, r.ok ? 200 : statusFor(r), r);
        return redirect(
          res,
          `${back}?${r.ok ? `done=${encodeURIComponent(r.replayed ? `${ok} (already made)` : ok)}` : `error=${encodeURIComponent(message(r))}`}`,
        );
      };
      const refuse = (status: number, error: string, back: string) =>
        json ? send(res, status, { ok: false, error }) : redirect(res, `${back}?error=${encodeURIComponent(error)}`);

      if (path === '/transfers' || path === '/api/transfers') {
        const amount = typeof b.amount === 'number' ? b.amount : parseAmount(String(b.amount ?? ''));
        if (!amount) return refuse(400, 'Enter an amount, such as 12.50.', '/transfers');
        if (!/^[A-Za-z0-9_-]{8,64}$/.test(key))
          return refuse(400, 'A transfer needs an idempotency key of 8 to 64 letters, digits, _ or -.', '/transfers');
        const from = String(b.from ?? '');
        // Whether the person may spend from `from` is asked of the database
        // under their own token: a customer's reads only the accounts their
        // claim lists. The block itself runs under the app's token.
        const [src] = await ctx.reader.rows<{ currency: string }>(
          'get accounts select currency where ext = $1 and kind = "customer" limit 1',
          [from],
        );
        if (!src) return refuse(404, 'There is no account of yours with that id.', '/transfers');
        const r = await ledger().transfer(
          {
            ref: `tr-${key}`,
            from,
            to: String(b.to ?? '').trim(),
            amount,
            currency: src.currency,
            memo: String(b.memo ?? '').slice(0, 140),
            actor: admin ? undefined : ctx.user.name,
          },
          key,
        );
        return done(r, '/transfers', 'Transfer sent');
      }
      if (path === '/refunds' || path === '/api/refunds') {
        const amount = typeof b.amount === 'number' ? b.amount : parseAmount(String(b.amount ?? ''));
        if (!amount) return refuse(400, 'Enter an amount, such as 12.50.', '/transfers');
        if (!/^[A-Za-z0-9_-]{8,128}$/.test(key)) return refuse(400, 'A refund needs an idempotency key.', '/transfers');
        const of = String(b.of ?? '');
        // A customer refunds only what was paid to an account they hold,
        // read under their token; an operator refunds anything.
        if (!admin) {
          const [t] = await ctx.reader.rows<{ dst: string }>('get transfers select dst where ref = $1 limit 1', [of]);
          if (!t || !ctx.accounts?.includes(t.dst)) return refuse(404, 'There is no payment to you with that ref.', '/transfers');
        }
        const r = await ledger().refund(
          { ref: `rf-${key}`, of, amount, memo: String(b.memo ?? 'Refund').slice(0, 140), actor: admin ? undefined : ctx.user.name },
          key,
        );
        return done(r, '/transfers', 'Refund sent');
      }
      // Below, the operator's actions.
      if (!admin) return json ? send(res, 403, { error: 'for operators' }) : page('Not found', '', v.notFound(), 404);
      const status = /^\/(?:api\/)?accounts\/([^/]+)\/status$/.exec(path);
      if (status) {
        const r = await ledger().setStatus(
          decodeURIComponent(status[1]),
          b.status === 'open' ? 'open' : 'frozen',
          ctx.user.name,
          String(b.why ?? ''),
        );
        return done(r, '/accounts', b.status === 'open' ? 'Account unfrozen' : 'Account frozen');
      }
      if (path === '/accounts' || path === '/api/accounts') {
        const holders = (Array.isArray(b.holders) ? b.holders : String(b.holders ?? '').split(','))
          .map((h) => String(h).trim())
          .filter(Boolean);
        const r = await ledger().open(
          { ext: String(b.ext ?? ''), name: String(b.name ?? ''), holders, currency: String(b.currency ?? '') },
          ctx.user.name,
        );
        return done(r, '/accounts', 'Account opened');
      }
      if (path === '/deposits' || path === '/api/deposits') {
        const amount = typeof b.amount === 'number' ? b.amount : parseAmount(String(b.amount ?? ''));
        if (!amount) return refuse(400, 'Enter an amount, such as 12.50.', '/accounts');
        if (!/^[A-Za-z0-9_-]{8,64}$/.test(key)) return refuse(400, 'A deposit needs an idempotency key.', '/accounts');
        const to = String(b.to ?? '');
        const [a] = await ctx.reader.rows<{ currency: string }>('get accounts select currency where ext = $1 limit 1', [to]);
        if (!a) return refuse(404, 'There is no such account.', '/accounts');
        const r = await ledger().deposit(
          { ref: `dep-${key}`, to, amount, currency: a.currency, memo: String(b.memo ?? 'Deposit').slice(0, 140) },
          key,
        );
        return done(r, '/accounts', 'Deposit made');
      }
      if (path === '/api/holds') {
        const [a] = await ctx.reader.rows<{ currency: string }>('get accounts select currency where ext = $1 limit 1', [
          String(b.account ?? ''),
        ]);
        if (!a) return send(res, 404, { error: 'no such account' });
        const r = await ledger().hold(
          {
            ref: `hd-${key}`,
            account: String(b.account),
            amount: Number(b.amount),
            currency: a.currency,
            ttlMs: Number(b.ttlMs ?? 15 * 60_000),
          },
          key,
        );
        return send(res, r.ok ? 200 : statusFor(r), r);
      }
      const hold = /^\/api\/holds\/([^/]+)\/(capture|release)$/.exec(path);
      if (hold) {
        const ref = decodeURIComponent(hold[1]);
        const r =
          hold[2] === 'capture'
            ? await ledger().capture(
                { hold: ref, ref: `cp-${key}`, to: String(b.to ?? ''), amount: Number(b.amount), memo: String(b.memo ?? '') },
                key,
              )
            : await ledger().release(ref);
        return send(res, r.ok ? 200 : statusFor(r), r);
      }
    }

    // ---- reads for the API
    if (method === 'GET' && path === '/api/accounts') return send(res, 200, await ctx.reader.rows('get accounts order ext limit 500'));
    if (method === 'GET' && path === '/api/transfers')
      return send(res, 200, await ctx.reader.rows('get transfers order at desc limit 200'));
    if (method === 'GET' && path === '/api/reconciliation') {
      if (!admin) return send(res, 403, { error: 'for operators' });
      return send(res, 200, await reconcile(ctx.reader));
    }
    return json ? send(res, 404, { error: 'not found' }) : page('Not found', '', v.notFound(), 404);
  }

  const server = createServer((req, res) => {
    handle(req, res).catch((e: unknown) => {
      const status = e instanceof StoreError ? (e.status === 403 || e.status === 404 ? 404 : 502) : 500;
      if (status >= 500) console.error(e);
      if (!res.headersSent) send(res, status, { error: status === 404 ? 'not found' : 'the ledger could not answer' });
    });
  });
  let timer: NodeJS.Timeout | null = null;
  const every = opts.reapEvery ?? 5000;
  if (every > 0) {
    timer = setInterval(
      () =>
        void ledger()
          .reap()
          .catch(() => {}),
      every,
    );
    timer.unref();
  }
  server.on('close', () => timer && clearInterval(timer));
  return Object.assign(server, { ledger });
}

/** What the sink file holds against the journal, and how far the consumer is behind. */
// The consumer's place is the operator's to read, and the console holds no
// operator token: the sink writes where it stands beside its file.
async function sinkState(file: string, journalEntries: number) {
  const { lines, entries } = readSink(file);
  const status = sinkStatus(file);
  const fresh = status && Date.now() - status.checked < 30_000;
  return { lines, entries: entries.size, matches: lines ? entries.size === journalEntries : null, behind: fresh ? status.behind : null };
}

function statusFor(r: Result): number {
  if (r.ok) return 200;
  switch (r.reason) {
    case 'invalid':
      return 400;
    case 'not_found':
    case 'not_holder':
      return 404;
    case 'rate_limited':
      return 429;
    default:
      return 409;
  }
}

function message(r: Result): string {
  if (r.ok) return '';
  switch (r.reason) {
    case 'funds':
      return 'Not enough available money in the account.';
    case 'recipient':
      return "The recipient account doesn't exist, is frozen, or holds another currency.";
    case 'source':
      return 'The account is frozen.';
    case 'rate_limited':
      return 'This account has made too many transfers in the last minute. Try again shortly.';
    case 'duplicate':
      return 'This was made already.';
    case 'refund_exceeds':
      return "That's more than is left to refund on this payment.";
    case 'hold':
      return 'The hold was captured, released or has lapsed.';
    case 'not_found':
      return 'There is nothing with that id.';
    case 'not_holder':
      return 'There is no account of yours with that id.';
    default:
      return r.error;
  }
}

function cookies(req: IncomingMessage): Record<string, string> {
  const out: Record<string, string> = {};
  for (const part of (req.headers.cookie ?? '').split(';')) {
    const eq = part.indexOf('=');
    if (eq > 0) out[part.slice(0, eq).trim()] = part.slice(eq + 1).trim();
  }
  return out;
}

async function body(req: IncomingMessage): Promise<Record<string, unknown>> {
  let text = '';
  for await (const chunk of req) {
    text += chunk;
    if (text.length > 64_000) throw new Error('body too large');
  }
  if (String(req.headers['content-type'] ?? '').startsWith('application/json')) {
    try {
      const v = JSON.parse(text || '{}');
      return v && typeof v === 'object' ? v : {};
    } catch {
      return {};
    }
  }
  return Object.fromEntries(new URLSearchParams(text));
}

function send(res: ServerResponse, status: number, value: unknown) {
  const text = typeof value === 'string' ? value : JSON.stringify(value);
  res.writeHead(status, {
    'content-type': typeof value === 'string' ? 'text/plain; charset=utf-8' : 'application/json',
    'cache-control': 'no-store',
  });
  res.end(text);
}

function html(res: ServerResponse, status: number, text: string) {
  res.writeHead(status, {
    'content-type': 'text/html; charset=utf-8',
    'cache-control': 'no-store',
    'content-security-policy': "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
    'x-content-type-options': 'nosniff',
    'referrer-policy': 'same-origin',
  });
  res.end(text);
}

function redirect(res: ServerResponse, to: string) {
  res.writeHead(303, { location: to, 'cache-control': 'no-store' });
  res.end();
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const port = Number(process.env.PORT ?? 3000);
  ledgerServer().listen(port, '127.0.0.1', () => console.log(`Quire on http://127.0.0.1:${port} (tenant ${TENANT})`));
}
