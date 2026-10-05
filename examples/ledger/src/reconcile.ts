// Reconciliation: the balances against the journal, at one moment.
//
// fenecdb has one writer and no snapshots of its own, so "one moment" is a
// lock: the reads go as one /batch, which runs under the write lock -- no
// write lands between the first read and the last -- and its `Fenec-Seq`
// names the change the database stood at. A read holds the lock only while
// it runs, so the job's cost to writers is its own time (README,
// "Measured"). The change stream gives the other way: a consumer that
// applies every write from /_changes in order holds the state at each
// `seq` and can reconcile there with no lock at all (src/sink.ts keeps the
// journal's half of that).
import type { Store } from './store.ts';

export interface Report {
  /** The change the snapshot stood at. */
  seq?: number;
  ms: number;
  accounts: number;
  entries: number;
  /**
   * Per currency: every balance's sum and every entry's (both zero, the
   * outside account included); what customer accounts hold, their entries'
   * sum, and the outside account's balance; what holds reserve, by the
   * accounts' counters and by the live holds.
   */
  currencies: {
    currency: string;
    balances: number;
    journal: number;
    customers: number;
    customerEntries: number;
    outside: number;
    held: number;
    holds: number;
  }[];
  /** Accounts whose balance is not the sum of their entries. */
  drift: { account: string; balance: number; journal: number }[];
  /** Accounts whose `held` is not the sum of their live holds. */
  heldDrift: { account: string; held: number; holds: number }[];
  /** Customer accounts below zero, or holding more than they have. */
  negative: string[];
  /** Movements refunded past their amount. */
  overRefunded: string[];
  ok: boolean;
}

type Row = Record<string, unknown>;

export async function reconcile(store: Store): Promise<Report> {
  const t0 = performance.now();
  const { results, seq } = await store.snapshot([
    'get accounts select ext, currency, balance, held, kind',
    'get journal select account, sum(amount), count(*) group account',
    'get holds select account, sum(amount) where state = "held" group account',
    'get transfers select ref where refunded > amount',
  ]);
  const [accounts, journal, holds, over] = results as Row[][];
  const ms = performance.now() - t0;

  const byAccount = new Map<string, number>();
  let entries = 0;
  for (const r of journal) {
    byAccount.set(r.account as string, Number(r['sum(amount)']));
    entries += Number(r.count);
  }
  const holdsBy = new Map(holds.map((r) => [r.account as string, Number(r['sum(amount)'])]));
  const cur = new Map<string, Report['currencies'][number]>();
  const drift: Report['drift'] = [];
  const heldDrift: Report['heldDrift'] = [];
  const negative: string[] = [];
  const seen = new Set<string>();
  for (const a of accounts) {
    const ext = a.ext as string;
    const balance = Number(a.balance);
    const held = Number(a.held);
    const j = byAccount.get(ext) ?? 0;
    const h = holdsBy.get(ext) ?? 0;
    seen.add(ext);
    const c = cur.get(a.currency as string) ?? {
      currency: a.currency as string,
      balances: 0,
      journal: 0,
      customers: 0,
      customerEntries: 0,
      outside: 0,
      held: 0,
      holds: 0,
    };
    c.balances += balance;
    c.journal += j;
    if (a.kind === 'customer') {
      c.customers += balance;
      c.customerEntries += j;
    } else c.outside += balance;
    c.held += held;
    c.holds += h;
    cur.set(c.currency, c);
    if (balance !== j) drift.push({ account: ext, balance, journal: j });
    if (held !== h) heldDrift.push({ account: ext, held, holds: h });
    if (a.kind === 'customer' && (balance < 0 || held < 0 || balance - held < 0)) negative.push(ext);
  }
  // An entry naming an account that does not exist is drift too.
  for (const [account, j] of byAccount) if (!seen.has(account)) drift.push({ account, balance: 0, journal: j });

  const currencies = [...cur.values()].sort((a, b) => a.currency.localeCompare(b.currency));
  const ok =
    currencies.every((c) => c.balances === 0 && c.journal === 0 && c.held === c.holds) &&
    !drift.length &&
    !heldDrift.length &&
    !negative.length &&
    !over.length;
  return {
    seq,
    ms,
    accounts: accounts.length,
    entries,
    currencies,
    drift,
    heldDrift,
    negative,
    overRefunded: over.map((r) => r.ref as string),
    ok,
  };
}

/** Every movement's entries sum to zero: the ids of those that do not. */
export async function unbalanced(store: Store): Promise<string[]> {
  const rows = await store.rows<Row>('get journal select tx, sum(amount), count(*) group tx');
  return rows.filter((r) => Number(r['sum(amount)']) !== 0 || Number(r.count) !== 2).map((r) => r.tx as string);
}
