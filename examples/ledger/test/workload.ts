// A random mix of everything a ledger is asked, from many clients at once,
// and the checks that must hold after it. Used by the invariant tests (in
// process and over HTTP), the crash test and the benchmark.
import assert from 'node:assert/strict';
import { Ledger, WORLD, type Result, type TransferInput } from '../src/ledger.ts';
import { reconcile, unbalanced } from '../src/reconcile.ts';
import type { Store } from '../src/store.ts';

export interface Account {
  ext: string;
  currency: string;
}

export interface World {
  accounts: Account[];
  frozen: Account[];
  /** What deposits put into the ledger, per currency. */
  deposited: Map<string, number>;
}

/** A small deterministic generator: a failing run can be run again. */
export function rng(seed: number) {
  let s = seed >>> 0 || 1;
  const next = () => {
    s ^= s << 13;
    s ^= s >>> 17;
    s ^= s << 5;
    return (s >>> 0) / 2 ** 32;
  };
  return {
    next,
    int: (lo: number, hi: number) => lo + Math.floor(next() * (hi - lo + 1)),
    pick: <T>(xs: T[]): T => xs[Math.floor(next() * xs.length)],
  };
}

/**
 * World accounts for `currencies` (with the operator's store), `n` customer
 * accounts opened and funded through the ledger, `frozen` more frozen.
 */
export async function prepare(
  ledger: Ledger,
  operator: Store,
  opts: { n: number; frozen?: number; currencies?: string[]; opening?: number; prefix?: string },
): Promise<World> {
  const currencies = opts.currencies ?? ['EUR', 'GBP'];
  const prefix = opts.prefix ?? 'acct';
  for (const c of currencies) {
    const out = await operator.batch(
      [
        'put accounts {ext: $1, name: $2, holders: [], currency: $3, balance: 0, held: 0, status: "open", kind: "world", opened: now()} if absent',
      ],
      [WORLD(c), `Outside (${c})`, c],
    );
    assert.ok(out.ok, !out.ok ? out.error : '');
  }
  const world: World = { accounts: [], frozen: [], deposited: new Map(currencies.map((c) => [c, 0])) };
  const total = opts.n + (opts.frozen ?? 0);
  // Eight at a time: fenec-server takes 100 connections unless told more.
  let next = 0;
  await Promise.all(
    Array.from({ length: Math.min(8, total) }, async () => {
      for (let i = next++; i < total; i = next++) {
        const a = { ext: `${prefix}-${i}`, currency: currencies[i % currencies.length] };
        ok(await ledger.open({ ...a, name: `Account ${i}`, holders: [`u${i}`] }, 'test'));
        const amount = opts.opening ?? 100_000;
        ok(await ledger.deposit({ ref: `open-${a.ext}`, to: a.ext, amount, currency: a.currency }));
        world.deposited.set(a.currency, world.deposited.get(a.currency)! + amount);
        if (i >= opts.n) {
          ok(await ledger.setStatus(a.ext, 'frozen', 'test'));
          world.frozen.push(a);
        } else world.accounts.push(a);
      }
    }),
  );
  // In the order they were made, whatever order they landed in.
  const order = (a: Account) => Number(a.ext.slice(a.ext.lastIndexOf('-') + 1));
  world.accounts.sort((a, b) => order(a) - order(b));
  world.frozen.sort((a, b) => order(a) - order(b));
  return world;
}

export function ok(r: Result): void {
  assert.ok(r.ok, r.ok ? '' : `${r.reason}: ${r.error}`);
}

export interface Tally {
  ops: number;
  /** Movements acknowledged as made, by ref. */
  made: Map<string, { kind: string; amount: number }>;
  /** Per key: answers that made it (not replays, not duplicates). */
  firsts: Map<string, number>;
  replays: number;
  refusals: Map<string, number>;
  /** Per hold: captures, releases and lapses that succeeded. */
  ended: Map<string, number>;
  holds: Set<string>;
  captured: number;
  refundRaces: number;
  /** Per block, ms. */
  latency: number[];
  /** Transfers sent whose answer never came: a crash's unknowns. */
  pending: Map<string, TransferInput>;
  /** Every transfer sent, by its key. */
  inputs: Map<string, TransferInput>;
}

export interface MixOptions {
  ops: number;
  workers: number;
  seed?: number;
  /** Holds last 50 to this many ms. */
  holdTtl?: number;
  /** Called after each acknowledged movement (the crash test records them). */
  onMade?: (ref: string) => void;
  /** Stop early, once this returns true. */
  stop?: () => boolean;
  /** Retries send the same key again: over HTTP an Idempotency-Key. */
  keyed?: boolean;
  /** An operation threw: true stops its worker, false throws on. */
  onError?: (e: unknown) => boolean;
}

/**
 * The mix: transfers (to missing, frozen and other-currency accounts too,
 * and overdrafts), retries of earlier ones with the same key (some sent
 * twice at once), concurrent refunds of one payment, holds captured,
 * released or left to lapse (and a capture racing a release), and a reaper
 * running beside it all.
 */
export async function mix(ledger: Ledger, world: World, o: MixOptions): Promise<Tally> {
  const t: Tally = {
    ops: 0,
    made: new Map(),
    firsts: new Map(),
    replays: 0,
    refusals: new Map(),
    ended: new Map(),
    holds: new Set(),
    captured: 0,
    refundRaces: 0,
    latency: [],
    pending: new Map(),
    inputs: new Map(),
  };
  const sent: { key: string; input: Parameters<Ledger['transfer']>[0] }[] = [];
  const paid: { ref: string; amount: number }[] = [];
  const byCurrency = new Map<string, Account[]>();
  for (const a of world.accounts) byCurrency.set(a.currency, [...(byCurrency.get(a.currency) ?? []), a]);
  const keyed = o.keyed ?? true;

  const note = (r: Result, key: string, kind: string, amount: number) => {
    if (r.ok) {
      if (r.replayed) t.replays++;
      else t.firsts.set(key, (t.firsts.get(key) ?? 0) + 1);
      t.made.set(r.ref, { kind, amount });
      o.onMade?.(r.ref);
    } else if (r.reason === 'duplicate') {
      t.replays++; // made before: in process, how a retry learns it
    } else t.refusals.set(r.reason, (t.refusals.get(r.reason) ?? 0) + 1);
  };
  const timed = async <T>(f: () => Promise<T>): Promise<T> => {
    const t0 = performance.now();
    try {
      return await f();
    } finally {
      t.latency.push(performance.now() - t0);
    }
  };

  let reaping = true;
  const reaper = (async () => {
    while (reaping) {
      try {
        for (const ref of await ledger.reap()) t.ended.set(ref, (t.ended.get(ref) ?? 0) + 1);
      } catch (e) {
        if (o.onError?.(e)) return;
        throw e;
      }
      await new Promise((r) => setTimeout(r, 25));
    }
  })();

  let next = 0;
  const holdTtl = o.holdTtl ?? 400;
  await Promise.all(
    Array.from({ length: o.workers }, async (_, w) => {
      const r = rng((o.seed ?? 7) * 1000 + w + 1);
      for (;;) {
        if (o.stop?.()) return;
        const i = next++;
        if (i >= o.ops) return;
        t.ops++;
        try {
          const roll = r.next();
          if (roll < 0.55 || !paid.length) {
            // A transfer, sometimes one that must be refused.
            const from = r.next() < 0.04 ? r.pick(world.frozen) : r.pick(world.accounts);
            const same = byCurrency.get(from.currency)!;
            const odd = r.next();
            const to =
              odd < 0.04
                ? { ext: `missing-${r.int(0, 99)}`, currency: from.currency }
                : odd < 0.08
                  ? r.pick(world.frozen)
                  : odd < 0.12
                    ? r.pick(world.accounts)
                    : r.pick(same);
            const amount = r.next() < 0.05 ? r.int(200_000, 2_000_000) : r.int(1, 9_000);
            const key = `k${o.seed ?? 7}-${i}`;
            const input = { ref: `tr-${key}`, from: from.ext, to: to.ext, amount, currency: from.currency, memo: `op ${i}` };
            if (to.ext === from.ext) continue;
            t.pending.set(key, input);
            t.inputs.set(key, input);
            const res = await timed(() => ledger.transfer(input, keyed ? key : undefined));
            t.pending.delete(key);
            note(res, key, 'transfer', amount);
            sent.push({ key, input });
            if (res.ok) paid.push({ ref: input.ref, amount });
          } else if (roll < 0.65 && sent.length) {
            // A retry of an earlier transfer, with its key; a third of them
            // sent twice at once.
            const { key, input } = r.pick(sent);
            const twice = r.next() < 0.33;
            const answers = await Promise.all(
              Array.from({ length: twice ? 2 : 1 }, () => timed(() => ledger.transfer(input, keyed ? key : undefined))),
            );
            for (const res of answers) note(res, key, 'transfer', input.amount);
          } else if (roll < 0.75) {
            // Refunds of one payment racing each other, each up to 60% of it.
            const p = r.pick(paid);
            t.refundRaces++;
            await Promise.all(
              Array.from({ length: 4 }, async (_, k) => {
                const key = `rf${o.seed ?? 7}-${i}-${k}`;
                const amount = Math.max(1, Math.floor((p.amount * r.int(20, 60)) / 100));
                note(
                  await timed(() => ledger.refund({ ref: `rf-${key}`, of: p.ref, amount }, keyed ? key : undefined)),
                  key,
                  'refund',
                  amount,
                );
              }),
            );
          } else if (roll < 0.97) {
            // A hold, then a capture, a release, a race of the two, or nothing:
            // the reaper gives it back once it lapses.
            const a = r.pick(world.accounts);
            const key = `hd${o.seed ?? 7}-${i}`;
            const amount = r.int(1, 5_000);
            const h = await timed(() =>
              ledger.hold(
                { ref: `hd-${key}`, account: a.ext, amount, currency: a.currency, ttlMs: r.int(50, holdTtl) },
                keyed ? key : undefined,
              ),
            );
            note(h, key, 'hold', amount);
            if (!h.ok) continue;
            t.holds.add(h.ref);
            const end = (res: Result, captured = false) => {
              if (res.ok) {
                t.ended.set(h.ref, (t.ended.get(h.ref) ?? 0) + 1);
                if (captured) {
                  t.captured++;
                  t.made.set(res.ref, { kind: 'capture', amount });
                }
              }
            };
            const fate = r.next();
            const to = r.pick(byCurrency.get(a.currency)!.filter((x) => x.ext !== a.ext));
            const capture = () =>
              timed(() =>
                ledger.capture({ hold: h.ref, ref: `cp-${key}`, to: to.ext, amount: r.int(1, amount) }, keyed ? `cp-${key}` : undefined),
              );
            if (fate < 0.35) end(await capture(), true);
            else if (fate < 0.6) end(await timed(() => ledger.release(h.ref)));
            else if (fate < 0.8) {
              const [c, rel] = await Promise.all([capture(), timed(() => ledger.release(h.ref))]);
              end(c, true);
              end(rel);
            }
          } else {
            // An account frozen and thawed again while transfers run.
            const a = r.pick(world.accounts);
            await ledger.setStatus(a.ext, 'frozen', 'test');
            await ledger.setStatus(a.ext, 'open', 'test');
          }
        } catch (e) {
          // A server killed under the load (the crash test): this worker stops.
          if (o.onError?.(e)) return;
          throw e;
        }
      }
    }),
  );
  // Every hold lapses within its ttl; the reaper gives the rest back.
  await new Promise((r) => setTimeout(r, holdTtl + 50));
  reaping = false;
  await reaper;
  if (o.stop?.()) return t; // stopped from outside: the caller settles the holds
  for (const ref of await ledger.reap()) t.ended.set(ref, (t.ended.get(ref) ?? 0) + 1);
  return t;
}

/** What must hold after a mix, read with the operator's store. */
export async function check(operator: Store, world: World, t: Tally, opts: { complete?: boolean; holds?: boolean } = {}): Promise<void> {
  // Balances against the journal, per account and per currency, in one snapshot.
  const report = await reconcile(operator);
  assert.deepEqual(report.drift, [], 'every balance is the sum of its entries');
  assert.deepEqual(report.heldDrift, [], "every account's held is the sum of its live holds");
  assert.deepEqual(report.negative, [], 'no customer account below zero');
  assert.deepEqual(report.overRefunded, [], 'no movement refunded past its amount');
  for (const c of report.currencies) {
    assert.equal(c.balances, 0, `${c.currency}: the balances, the outside account's included, sum to zero`);
    assert.equal(c.journal, 0, `${c.currency}: the entries sum to zero`);
  }
  assert.ok(report.ok);
  assert.deepEqual(await unbalanced(operator), [], "every movement's two entries sum to zero");

  // The money in customer accounts is what was deposited, per currency.
  const sums = await operator.rows<{ currency: string; 'sum(balance)': number }>(
    'get accounts select currency, sum(balance) where kind = "customer" group currency',
  );
  for (const s of sums) assert.equal(s['sum(balance)'], world.deposited.get(s.currency), `${s.currency}: the sum is constant`);

  // Each key made at most one movement, and every acknowledged one is there.
  for (const [key, n] of t.firsts) assert.ok(n <= 1, `${key} made ${n} movements`);
  const refs = new Set((await operator.rows<{ ref: string }>('get transfers select ref')).map((r) => r.ref));
  for (const ref of t.made.keys()) if (!ref.startsWith('hd-')) assert.ok(refs.has(ref), `acknowledged ${ref} is in the ledger`);
  if (opts.complete !== false) {
    const movements = [...refs].filter((r) => !r.startsWith('open-')).length;
    const made = [...t.made.entries()].filter(([ref]) => !ref.startsWith('hd-')).length;
    assert.equal(movements, made, 'the ledger holds exactly the movements acknowledged');
  }

  // Refunds of each payment: what the counter says, never past the payment.
  const refunds = await operator.rows<{ of: string; 'sum(amount)': number }>(
    'get transfers select of, sum(amount) where kind = "refund" group of',
  );
  const originals = new Map(
    (
      await operator.rows<{ ref: string; amount: number; refunded: number }>(
        'get transfers select ref, amount, refunded where refunded > 0',
      )
    ).map((r) => [r.ref, r]),
  );
  for (const r of refunds) {
    const o = originals.get(r.of);
    assert.ok(o, `${r.of} was refunded`);
    assert.equal(o.refunded, r['sum(amount)'], `${r.of}: its counter is its refunds' sum`);
    assert.ok(o.refunded <= o.amount, `${r.of}: refunded ${o.refunded} of ${o.amount}`);
  }

  // Every hold ended exactly once, and none is left holding money.
  assert.equal((await operator.rows('get holds where state = "held"')).length, 0, 'no hold left held');
  if (opts.holds === false) return;
  for (const ref of t.holds) assert.equal(t.ended.get(ref) ?? 0, 1, `${ref} ended ${t.ended.get(ref) ?? 0} times`);
  const captured = await operator.rows('get holds select ref where state = "captured"');
  assert.equal(captured.length, t.captured, 'captures acknowledged are the holds captured');
}

export function percentile(xs: number[], p: number): number {
  if (!xs.length) return 0;
  const s = [...xs].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor((p / 100) * s.length))];
}
