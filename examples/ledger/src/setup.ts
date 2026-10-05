// A tenant made ready: created on the node, its schema applied, a world
// account for each currency, and -- with `demo` -- the console's people and
// a few accounts with money in them. Runs with the operator's token and the
// node's admin token; everything after the schema goes through the ledger
// as the console's server would, under the app's scoped token.
import { ADMIN_TOKEN, CURRENCIES, FENEC_URL, OPERATOR_TOKEN, SCHEMA, tenantUrl } from './config.ts';
import { Ledger, WORLD } from './ledger.ts';
import { HttpStore } from './store.ts';
import { Tokens } from './tokens.ts';
import { hashPassword } from './users.ts';

export async function createTenant(tenant: string, node = FENEC_URL): Promise<void> {
  const res = await fetch(`${node}/_admin/tenants/${tenant}`, { method: 'PUT', headers: { authorization: `Bearer ${ADMIN_TOKEN}` } });
  if (!res.ok && res.status !== 409) throw new Error(`create tenant ${tenant}: ${res.status} ${await res.text()}`);
  const base = `${node}/t/${tenant}`;
  const applied = await fetch(`${base}/_schema/apply`, {
    method: 'POST',
    headers: { authorization: `Bearer ${OPERATOR_TOKEN}`, 'content-type': 'application/json' },
    body: JSON.stringify({ format: 1, fenecql: SCHEMA }),
  });
  if (!applied.ok) throw new Error(`schema of ${tenant}: ${applied.status} ${await applied.text()}`);
  const root = new HttpStore(base, OPERATOR_TOKEN);
  for (const c of CURRENCIES) {
    // The world account is the operator's to make: the app's token may
    // insert an account only at zero and only a customer's (policy.txt).
    const out = await root.batch(
      [
        `put accounts {ext: $1, name: $2, holders: [], currency: $3, balance: 0, held: 0, status: "open", kind: "world", opened: now()} if absent`,
      ],
      [WORLD(c), `Outside the ledger (${c})`, c],
    );
    if (!out.ok) throw new Error(`world account ${c}: ${out.error}`);
  }
}

export const DEMO_PASSWORD = process.env.LEDGER_DEMO_PASSWORD ?? 'ledger-demo';

export async function seedDemo(tenant: string): Promise<void> {
  const root = new HttpStore(tenantUrl(tenant), OPERATOR_TOKEN);
  if ((await root.rows('get users limit 1')).length) return;
  const people = [
    { name: 'ops', display: 'Imogen Hale', role: 'admin' },
    { name: 'ada', display: 'Ada Brennan', role: 'customer' },
    { name: 'ben', display: 'Ben Okafor', role: 'customer' },
    { name: 'cleo', display: 'Cleo Marchetti', role: 'customer' },
    { name: 'dev', display: 'Dev Raman', role: 'customer' },
  ];
  for (const p of people) {
    const out = await root.batch(
      ['insert users {name: $1, display: $2, hash: $3, role: $4}'],
      [p.name, p.display, hashPassword(DEMO_PASSWORD), p.role],
    );
    if (!out.ok) throw new Error(`user ${p.name}: ${out.error}`);
  }

  const ledger = new Ledger(new HttpStore(tenantUrl(tenant), new Tokens(tenant).app()), { rateLimit: 1000 });
  const accounts = [
    { ext: 'ada-main', name: 'Ada Brennan, current', holders: ['ada'], currency: 'EUR' },
    { ext: 'ada-ben-home', name: 'Brennan & Okafor, household', holders: ['ada', 'ben'], currency: 'EUR' },
    { ext: 'ben-main', name: 'Ben Okafor, current', holders: ['ben'], currency: 'EUR' },
    { ext: 'ben-gbp', name: 'Ben Okafor, sterling', holders: ['ben'], currency: 'GBP' },
    { ext: 'cleo-studio', name: 'Marchetti Studio', holders: ['cleo'], currency: 'EUR' },
    { ext: 'cleo-usd', name: 'Marchetti Studio, dollars', holders: ['cleo'], currency: 'USD' },
    { ext: 'dev-main', name: 'Dev Raman, current', holders: ['dev'], currency: 'GBP' },
  ];
  for (const a of accounts) must(await ledger.open(a, 'ops'));
  const deposits: [string, number, string][] = [
    ['ada-main', 420_000, 'EUR'],
    ['ada-ben-home', 185_050, 'EUR'],
    ['ben-main', 96_500, 'EUR'],
    ['ben-gbp', 54_000, 'GBP'],
    ['cleo-studio', 1_250_000, 'EUR'],
    ['cleo-usd', 310_000, 'USD'],
    ['dev-main', 72_480, 'GBP'],
  ];
  for (const [to, amount, currency] of deposits) {
    must(await ledger.deposit({ ref: `dep-${to}-1`, to, amount, currency, memo: 'Opening deposit' }));
  }
  const transfers: [string, string, number, string][] = [
    ['ada-main', 'ada-ben-home', 60_000, 'Rent share, October'],
    ['ben-main', 'ada-ben-home', 60_000, 'Rent share, October'],
    ['ada-main', 'cleo-studio', 18_900, 'Logo design, invoice 2231'],
    ['ada-ben-home', 'cleo-studio', 4_250, 'Framing'],
    ['cleo-studio', 'ben-main', 12_000, 'Photography, two days'],
  ];
  let i = 0;
  for (const [from, to, amount, memo] of transfers) {
    must(await ledger.transfer({ ref: `seed-tr-${++i}`, from, to, amount, currency: 'EUR', memo }));
  }
  must(await ledger.refund({ ref: 'seed-rf-1', of: 'seed-tr-4', amount: 1_250, memo: 'One frame arrived cracked' }));
  must(await ledger.hold({ ref: 'seed-hold-1', account: 'ben-gbp', amount: 8_000, currency: 'GBP', ttlMs: 7 * 24 * 3600_000 }));
}

function must(r: { ok: boolean; error?: string }) {
  if (!r.ok) throw new Error(`seed: ${r.error}`);
}
