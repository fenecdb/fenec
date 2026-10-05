// Who can read and move what. Each refusal is asked of fenec-server
// itself, with the token the attacker would hold, and of the console's
// API: the database refuses, not only the code in front of it.
import assert from 'node:assert/strict';
import type { AddressInfo } from 'node:net';
import { after, before, describe, test } from 'node:test';
import { createHmac } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { Ledger } from '../src/ledger.ts';
import { ledgerServer } from '../src/server.ts';
import { DEMO_PASSWORD, seedDemo } from '../src/setup.ts';
import { HttpStore } from '../src/store.ts';
import { JWT_SECRET, mint, Tokens } from '../src/tokens.ts';
import { FENEC_URL, freshTenant, query, raw } from './helpers.ts';

let tenant: string;
let other: string;
let tokens: Tokens;
let console_: ReturnType<typeof ledgerServer>;
let site: string;

before(async () => {
  ({ tenant, tokens } = await freshTenant('sec'));
  ({ tenant: other } = await freshTenant('sec-other'));
  await seedDemo(tenant);
  await seedDemo(other);
  console_ = ledgerServer({ tenant, rateLimit: 5, reapEvery: 0, insecureCookies: true });
  await new Promise<void>((r) => console_.listen(0, '127.0.0.1', r));
  site = `http://127.0.0.1:${(console_.address() as AddressInfo).port}`;
});
after(() => console_.close());

const ada = () => tokens.customer('ada', ['ada-main', 'ada-ben-home']);
const ben = () => tokens.customer('ben', ['ben-main', 'ben-gbp', 'ada-ben-home']);

/** A signed-in console session. */
async function signIn(
  name: string,
): Promise<(path: string, body?: unknown, headers?: Record<string, string>) => Promise<{ status: number; body: unknown }>> {
  const res = await fetch(`${site}/api/login`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ name, password: DEMO_PASSWORD }),
  });
  assert.equal(res.status, 200);
  const cookie = res.headers.getSetCookie()[0].split(';')[0];
  return async (path, body, headers = {}) => {
    const r = await fetch(`${site}${path}`, {
      method: body === undefined ? 'GET' : 'POST',
      headers: { cookie, ...(body === undefined ? {} : { 'content-type': 'application/json' }), ...headers },
      body: body === undefined ? undefined : JSON.stringify(body),
      redirect: 'manual',
    });
    const text = await r.text();
    let b: unknown = text;
    try {
      b = JSON.parse(text);
    } catch {
      // a page
    }
    return { status: r.status, body: b };
  };
}

describe('a customer sees only their own money', () => {
  test('reads through fenec-server: accounts, movements, entries and holds are their own, joint ones included', async () => {
    const accounts = (await query(tenant, ada(), 'get accounts select ext order ext')).body as { ext: string }[];
    assert.deepEqual(
      accounts.map((a) => a.ext),
      ['ada-ben-home', 'ada-main'],
    );
    const bens = (await query(tenant, ben(), 'get accounts select ext order ext')).body as { ext: string }[];
    assert.deepEqual(
      bens.map((a) => a.ext),
      ['ada-ben-home', 'ben-gbp', 'ben-main'],
    );
    const transfers = (await query(tenant, ada(), 'get transfers select src, dst')).body as { src: string; dst: string }[];
    assert.ok(transfers.length > 0);
    for (const t of transfers) assert.ok(['ada-main', 'ada-ben-home'].includes(t.src) || ['ada-main', 'ada-ben-home'].includes(t.dst));
    const entries = (await query(tenant, ada(), 'get journal select account')).body as { account: string }[];
    assert.ok(entries.length > 0 && entries.every((e) => ['ada-main', 'ada-ben-home'].includes(e.account)));
    // Counts, sums and filters naming another's account see none of it.
    assert.deepEqual((await query(tenant, ada(), 'get accounts where ext = "cleo-studio"')).body, []);
    assert.deepEqual((await query(tenant, ada(), 'get journal where account = "cleo-studio" count')).body, [{ count: 0 }]);
    assert.deepEqual((await query(tenant, ada(), 'get holds')).body, []); // the one hold is ben's
    assert.equal(((await query(tenant, ben(), 'get holds')).body as unknown[]).length, 1);
    // Collections no rule names for a customer do not exist for them.
    for (const c of ['users', 'events', 'limits']) assert.equal((await query(tenant, ada(), `get ${c}`)).status, 404, c);
  });

  test('a customer token moves no money by any route', async () => {
    const before_ = (await query(tenant, ada(), 'get accounts select ext, balance order ext')).body;
    const writes = [
      'set accounts {balance: balance + 100000} where ext = "ada-main"',
      'set accounts {balance: 0} where ext = "cleo-studio"',
      'insert journal {entry: "x:dr", tx: "x", account: "ada-main", currency: "EUR", amount: 100000, kind: "transfer", at: now()}',
      'insert transfers {ref: "x", kind: "transfer", src: "cleo-studio", dst: "ada-main", currency: "EUR", amount: 1, refunded: 0}',
      'del transfers where src = "ada-main"',
    ];
    for (const w of writes) assert.equal((await query(tenant, ada(), w)).status, 403, w);
    const batch = await raw(`/t/${tenant}/batch`, {
      method: 'POST',
      token: ada(),
      body: [
        `set accounts {balance: balance - 1} where ext = "cleo-studio" require 1`,
        `set accounts {balance: balance + 1} where ext = "ada-main" require 1`,
      ]
        .map((q) => JSON.stringify({ query: q }))
        .join('\n'),
    });
    assert.equal(batch.status, 403);
    assert.equal(
      (await raw(`/t/${tenant}/accounts?ext=eq.ada-main`, { method: 'PATCH', token: ada(), body: '{"balance":1}' })).status,
      403,
    );
    assert.equal((await raw(`/t/${tenant}/accounts`, { method: 'POST', token: ada(), body: '{"ext":"mine","balance":5}' })).status, 403);
    assert.equal((await raw(`/t/${tenant}/_changes?since=0`, { token: ada() })).status, 403, 'the change stream is the operator’s');
    assert.deepEqual((await query(tenant, ada(), 'get accounts select ext, balance order ext')).body, before_);
  });

  test('through the console: another holder’s account cannot be read, spent, refunded or frozen', async () => {
    const as = await signIn('ada');
    const accounts = (await as('/api/accounts')).body as { ext: string }[];
    assert.deepEqual(accounts.map((a) => a.ext).sort(), ['ada-ben-home', 'ada-main']);
    const page = await as('/accounts/cleo-studio');
    assert.equal(page.status, 404);
    const cleo = (await query(tenant, tokens.app(), 'get accounts select balance where ext = "cleo-studio"')).body;
    // Spending from an account she does not hold.
    const spent = await as('/api/transfers', { from: 'cleo-studio', to: 'ada-main', amount: 100 }, { 'idempotency-key': 'steal-0001' });
    assert.equal(spent.status, 404);
    // Refunding a payment made to someone else, back to herself.
    const refund = await as('/api/refunds', { of: 'seed-tr-3', amount: 100 }, { 'idempotency-key': 'steal-0002' });
    assert.equal(refund.status, 404, 'seed-tr-3 was paid to cleo, not to ada');
    // The operator's actions.
    assert.equal((await as('/api/accounts/cleo-studio/status', { status: 'frozen' })).status, 403);
    assert.equal((await as('/api/deposits', { to: 'ada-main', amount: 100 }, { 'idempotency-key': 'steal-0003' })).status, 403);
    assert.equal((await as('/api/reconciliation')).status, 403);
    assert.deepEqual((await query(tenant, tokens.app(), 'get accounts select balance where ext = "cleo-studio"')).body, cleo);
    // Her own account she can spend from, and a joint one too.
    const own = await as(
      '/api/transfers',
      { from: 'ada-ben-home', to: 'cleo-studio', amount: 250, memo: 'Joint, fine' },
      { 'idempotency-key': 'joint-0001' },
    );
    assert.equal(own.status, 200, JSON.stringify(own.body));
  });

  test('the console refuses a write from another origin, a forged cookie and no session', async () => {
    const as = await signIn('ada');
    const cross = await as(
      '/api/transfers',
      { from: 'ada-main', to: 'cleo-studio', amount: 1 },
      { origin: 'https://evil.example', 'idempotency-key': 'xorigin-01' },
    );
    assert.equal(cross.status, 403);
    const forged = await fetch(`${site}/api/accounts`, {
      headers: { cookie: 'quire_session=eyJzdWIiOiJvcHMiLCJyb2xlIjoiYWRtaW4ifQ.forged' },
    });
    assert.equal(forged.status, 401);
    assert.equal((await fetch(`${site}/api/accounts`)).status, 401);
    const html = await fetch(`${site}/accounts`, { redirect: 'manual' });
    assert.equal(html.status, 303);
  });
});

describe('the block checks who asks', () => {
  test('a transfer made for a customer requires, inside its block, that they hold the source', async () => {
    // As if the console had a bug that skipped its own check: the block
    // still refuses, under the same lock as the debit.
    const ledger = new Ledger(new HttpStore(`${FENEC_URL}/t/${tenant}`, tokens.app()), { rateLimit: 1000 });
    const theft = await ledger.transfer({ ref: 'actor-1-xxxx', from: 'cleo-studio', to: 'ada-main', amount: 100, currency: 'EUR', actor: 'ada' });
    assert.equal(!theft.ok && theft.reason, 'not_holder');
    const joint = await ledger.transfer({ ref: 'actor-2-xxxx', from: 'ada-ben-home', to: 'cleo-studio', amount: 100, currency: 'EUR', actor: 'ben' });
    assert.ok(joint.ok, 'ben holds the joint account');
    const refund = await ledger.refund({ ref: 'actor-3-xxxx', of: 'seed-tr-1', amount: 100, actor: 'cleo' });
    assert.equal(!refund.ok && refund.reason, 'not_holder', 'seed-tr-1 was paid to ada and ben, not to cleo');
  });
});

describe('the journal cannot be rewritten', () => {
  const rewrite = (tx: string) => [
    `set journal {amount: 1} where tx = "${tx}"`,
    `del journal where tx = "${tx}"`,
    'del journal',
    `set events {what: "nothing"} where account = "ada-main"`,
    'del events',
  ];

  test('the app token: 403 on every path, /query, /batch and REST', async () => {
    const app = tokens.app();
    for (const q of rewrite('seed-tr-1')) assert.equal((await query(tenant, app, q)).status, 403, q);
    for (const q of rewrite('seed-tr-1')) {
      const b = await raw(`/t/${tenant}/batch`, {
        method: 'POST',
        token: app,
        body: JSON.stringify({ query: 'get journal limit 1' }) + '\n' + JSON.stringify({ query: q }),
      });
      assert.equal(b.status, 403, `batch: ${q}`);
    }
    assert.equal((await raw(`/t/${tenant}/journal?tx=eq.seed-tr-1`, { method: 'PATCH', token: app, body: '{"amount":1}' })).status, 403);
    assert.equal((await raw(`/t/${tenant}/journal?tx=eq.seed-tr-1`, { method: 'DELETE', token: app })).status, 403);
    // A `put` naming an entry's id would write over it: refused as well.
    assert.equal((await query(tenant, app, 'put journal {id: 1, entry: "x", amount: 5}')).status, 403);
    // And the schema: no drop, no alter, no compact.
    for (const q of ['drop collection journal', 'alter collection journal drop field amount', 'compact journal'])
      assert.equal((await query(tenant, app, q)).status, 403, q);
    const apply = await raw(`/t/${tenant}/_schema/apply`, { method: 'POST', token: app, body: '{"format":1,"fenecql":""}' });
    assert.equal(apply.status, 403);
    // The entries are what they were.
    const legs = (await query(tenant, app, 'get journal select amount where tx = "seed-tr-1" order amount')).body;
    assert.deepEqual(legs, [{ amount: -60000 }, { amount: 60000 }]);
  });

  test('the app token appends only balanced, zero-balance accounts; it cannot mint money into a new one', async () => {
    const app = tokens.app();
    const r = await query(
      tenant,
      app,
      'insert accounts {ext: "rich", name: "x", holders: [], currency: "EUR", balance: 1000000, held: 0, status: "open", kind: "customer"}',
    );
    assert.equal(r.status, 403);
    const w = await query(
      tenant,
      app,
      'insert accounts {ext: "w2", name: "x", holders: [], currency: "EUR", balance: 0, held: 0, status: "open", kind: "world"}',
    );
    assert.equal(w.status, 403, 'only the operator opens an outside account');
    assert.equal((await query(tenant, app, 'del accounts where ext = "ada-main"')).status, 403);
  });

  test('an operator’s admin token reads everything and writes nothing', async () => {
    const admin = tokens.admin('ops');
    assert.ok(((await query(tenant, admin, 'get accounts')).body as unknown[]).length >= 7);
    for (const q of ['set accounts {status: "frozen"} where ext = "ada-main"', 'insert journal {entry: "y"}', 'del holds'])
      assert.equal((await query(tenant, admin, q)).status, 403, q);
  });

  test('a compromised app that edits a balance is caught by reconciliation, and the journal shows the truth', async () => {
    const { tokens: t2, operator } = await freshTenant('sec-drift');
    await seedDemo(operator.base.split('/t/')[1]);
    const app = t2.app();
    // What an attacker holding the app's token can do: change a cached balance.
    assert.equal(
      (await query(operator.base.split('/t/')[1], app, 'set accounts {balance: balance + 500000} where ext = "dev-main"')).status,
      200,
    );
    const { reconcile } = await import('../src/reconcile.ts');
    const report = await reconcile(new HttpStore(operator.base, t2.admin('ops')));
    assert.equal(report.ok, false);
    assert.deepEqual(report.drift, [{ account: 'dev-main', balance: 72_480 + 500_000, journal: 72_480 }]);
  });
});

describe('tokens', () => {
  const tenantClaim = () => ({ sub: 'ledger-app', role: 'app', tenant });

  test('forged, expired, unexpiring and unsigned tokens are refused (401)', async () => {
    const forged = mint(tenantClaim(), 600, 'not-the-secret-not-the-secret-not-the-secret');
    const expired = mint({ ...tenantClaim(), exp: Math.floor(Date.now() / 1000) - 60 }, 600);
    const b64 = (o: unknown) => Buffer.from(JSON.stringify(o)).toString('base64url');
    const head = b64({ alg: 'HS256', typ: 'JWT' });
    const noExpBody = b64({ ...tenantClaim(), iat: Math.floor(Date.now() / 1000) });
    const noExp = `${head}.${noExpBody}.${createHmac('sha256', JWT_SECRET).update(`${head}.${noExpBody}`).digest('base64url')}`;
    const none = `${b64({ alg: 'none', typ: 'JWT' })}.${b64({ ...tenantClaim(), exp: 9999999999 })}.`;
    const tampered = (() => {
      const [h, , s] = tokens.customer('ada', ['ada-main']).split('.');
      return `${h}.${b64({ sub: 'ada', tenant, accounts: ['ada-main', 'cleo-studio'], exp: 9999999999 })}.${s}`;
    })();
    for (const [name, token] of Object.entries({ forged, expired, noExp, none, tampered })) {
      assert.equal((await query(tenant, token, 'get accounts')).status, 401, name);
    }
    if (process.env.FENEC_AUDIT) {
      // The node's audit log has a line for each refusal.
      const refused = readFileSync(process.env.FENEC_AUDIT, 'utf8')
        .split('\n')
        .filter((l) => l.includes('"event":"refused"') && l.includes(`/t/${tenant}/query`));
      assert.ok(refused.length >= 5, `${refused.length} refusals logged`);
    }
  });

  test('a token is bound to its tenant: the same subject and role reach no other tenant', async () => {
    for (const token of [tokens.app(), tokens.admin('ops'), ada()]) {
      for (const path of [`/t/${other}/query`, `/t/${other}/batch`]) {
        const r = await raw(path, { method: 'POST', token, body: JSON.stringify({ query: 'get accounts' }) });
        assert.equal(r.status, 403, path);
      }
      assert.equal((await raw(`/t/${other}/accounts`, { token })).status, 403);
      assert.equal((await raw(`/t/${other}/accounts/changes`, { token })).status, 403);
    }
    // A token naming no tenant reaches none.
    const unbound = mint({ sub: 'ledger-app', role: 'app' }, 600);
    assert.equal((await query(tenant, unbound, 'get accounts')).status, 403);
    // The other tenant's ledger is untouched by a transfer here, and
    // its own app token works there.
    const there = new Tokens(other);
    assert.equal((await query(other, there.app(), 'get accounts limit 1')).status, 200);
  });
});

describe('the transfer rate limit', () => {
  test('an account makes at most its limit of transfers a minute; others are not held back', async () => {
    const { tenant: t, tokens: tk, operator } = await freshTenant('rate');
    await seedDemo(t);
    const ledger = new Ledger(new HttpStore(operator.base, tk.app()), { rateLimit: 5 });
    const results = await Promise.all(
      Array.from({ length: 12 }, (_, i) =>
        ledger.transfer({ ref: `rl-${i}-xxxx`, from: 'dev-main', to: 'ben-gbp', amount: 100, currency: 'GBP' }, `rl-key-${i}`),
      ),
    );
    assert.equal(results.filter((r) => r.ok).length, 5);
    assert.equal(results.filter((r) => !r.ok && r.reason === 'rate_limited').length, 7);
    // Another account's window is its own.
    assert.ok((await ledger.transfer({ ref: 'rl-other-1', from: 'cleo-studio', to: 'ada-main', amount: 100, currency: 'EUR' })).ok);
    // A refused transfer used none of the window: the counter is put back with its block.
    const window = (await query(t, tk.app(), 'get limits select n where account = "dev-main"')).body;
    assert.deepEqual(window, [{ n: 5 }]);
  });

  test('through the console, the sixth transfer in a minute is 429', async () => {
    const as = await signIn('dev');
    const codes: number[] = [];
    for (let i = 0; i < 6; i++)
      codes.push(
        (await as('/api/transfers', { from: 'dev-main', to: 'ben-gbp', amount: 1 }, { 'idempotency-key': `dev-rate-${i}-0000` })).status,
      );
    assert.deepEqual(codes, [200, 200, 200, 200, 200, 429]);
  });
});
