// The ledger: every movement of money as one block of FenecQL statements.
//
// Each block is a debit where the money is, a credit where it goes, two
// journal entries summing to zero, and a record of the movement, sent as
// one /batch. Each condition is part of the block: a `get ... require 1`
// that reads without writing ("the recipient is open, in this currency"),
// or a write ending in `require 1`, whose count is the write's own, taken
// under the lock that wrote it. A block whose condition fails is refused
// with 412, `at` naming the statement, and nothing of it lands. There is
// one writer, and the block holds the lock: nothing comes between a check
// and the write it guards.
import type { Outcome, Store } from './store.ts';

export type Refusal =
  | 'rate_limited' // too many transfers from the account in its window
  | 'recipient' // missing, frozen, or in another currency
  | 'source' // missing, frozen, or in another currency
  | 'funds' // balance less holds is under the amount
  | 'duplicate' // a movement with this ref exists already
  | 'refund_exceeds' // more than what is left of the original
  | 'hold' // not held any more: captured, released or lapsed
  | 'not_found'
  | 'not_holder' // the person it is made for holds no such account
  | 'invalid';

export type Result =
  { ok: true; ref: string; replayed: boolean; seq?: number } | { ok: false; reason: Refusal; status: number; error: string };

export interface TransferInput {
  /** The movement's ref, unique; an `Idempotency-Key` derives it. */
  ref: string;
  from: string;
  to: string;
  /** Minor units. */
  amount: number;
  currency: string;
  memo?: string;
  /**
   * Who asks, for a customer's transfer: the block then also requires that
   * they hold the source account, read under the same lock as the debit.
   */
  actor?: string;
}

/** A step of a block: its statement, and what its failure means. */
type Step = [statement: string, why: Refusal];

/** The account money enters and leaves the ledger through, one a currency. */
export const WORLD = (currency: string) => `world:${currency.toLowerCase()}`;

/**
 * A guard that reads and writes nothing: the account `$n` names is a
 * customer's, open, in the movement's currency ($4).
 */
const usable = (n: number): string =>
  `get accounts select ext where ext = $${n} and status = "open" and currency = $4 and kind = "customer" limit 1 require 1`;

export class Ledger {
  /** Transfers an account may make in a window of a minute. */
  readonly rateLimit: number;

  constructor(
    readonly store: Store,
    opts: { rateLimit?: number } = {},
  ) {
    this.rateLimit = opts.rateLimit ?? 30;
  }

  /**
   * Money from one customer account to another. The first two statements
   * are the counter recipe: the window's row made if there is none (it
   * expires a minute after it was made, `@ttl`), then counted only while
   * under the limit. The window lives in the row, not in its key, so a
   * retry a minute later sends the same body, which its `Idempotency-Key`
   * still matches. With an `actor`, a guard that writes nothing requires
   * that they still hold the source: a holder taken off the account a
   * moment before cannot spend from it.
   */
  transfer(t: TransferInput, key?: string): Promise<Result> {
    const bad = invalid(t.amount, t.ref) ?? (t.from === t.to ? 'the source and the recipient are one account' : null);
    if (bad) return Promise.resolve(refused('invalid', 400, bad));
    return this.#move(
      [
        ['put limits {account: $1, n: 0, at: now()} if absent', 'invalid'],
        ['set limits {n: n + 1} where account = $1 and n < $10 require 1', 'rate_limited'],
        [usable(2), 'recipient'],
        [usable(1), 'source'],
        ['set accounts {balance: balance - $3} where ext = $1 and balance - held >= $3 require 1', 'funds'],
        ['set accounts {balance: balance + $3} where ext = $2 require 1', 'recipient'],
        ...holds(t.actor, 1),
      ],
      [t.from, t.to, t.amount, t.currency, t.ref, 'transfer', t.memo ?? ''],
      t.actor ? [this.rateLimit, t.actor] : [this.rateLimit],
      key,
    );
  }

  /**
   * Money into a customer account from outside: debited from the
   * currency's world account, which goes negative by what is in the ledger,
   * so every currency's balances sum to zero.
   */
  deposit(t: Omit<TransferInput, 'from'>, key?: string): Promise<Result> {
    const bad = invalid(t.amount, t.ref);
    if (bad) return Promise.resolve(refused('invalid', 400, bad));
    return this.#move(
      [
        [usable(2), 'recipient'],
        ['set accounts {balance: balance - $3} where ext = $1 and kind = "world" and currency = $4 require 1', 'source'],
        ['set accounts {balance: balance + $3} where ext = $2 require 1', 'recipient'],
      ],
      [WORLD(t.currency), t.to, t.amount, t.currency, t.ref, 'deposit', t.memo ?? ''],
      [],
      key,
    );
  }

  /**
   * Gives back part or all of a transfer or a capture, as a new movement
   * the other way: the original and its entries are never edited, only
   * its counter of what was refunded moves. That counter is the guard --
   * `refunded + amount <= amount`, under the write lock -- so refunds of
   * one payment racing each other never give back more than it was.
   * `kind: 'reversal'` is an operator's correction of the same shape.
   */
  async refund(
    r: { ref: string; of: string; amount: number; memo?: string; kind?: 'refund' | 'reversal'; actor?: string },
    key?: string,
  ): Promise<Result> {
    const bad = invalid(r.amount, r.ref);
    if (bad) return refused('invalid', 400, bad);
    const [orig] = await this.store.rows<{ src: string; dst: string; currency: string }>(
      'get transfers select src, dst, currency where ref = $1 and kind in ["transfer", "capture"] limit 1',
      [r.of],
    );
    if (!orig) return refused('not_found', 404, `no transfer or capture ${r.of}`);
    return this.#move(
      [
        [
          'set transfers {refunded: refunded + $3} where ref = $10 and src = $2 and dst = $1 and kind in ["transfer", "capture"] and refunded + $3 <= amount require 1',
          'refund_exceeds',
        ],
        ['set accounts {balance: balance - $3} where ext = $1 and balance - held >= $3 require 1', 'funds'],
        ['set accounts {balance: balance + $3} where ext = $2 require 1', 'recipient'],
        // A customer refunds only what was paid to an account they hold.
        ...holds(r.actor, 1),
      ],
      [orig.dst, orig.src, r.amount, orig.currency, r.ref, r.kind ?? 'refund', r.memo ?? ''],
      r.actor ? [r.of, r.actor] : [r.of],
      key,
    );
  }

  /**
   * Reserves `amount` of an account until `ttlMs` from now: the account's
   * `held` counter grows under the guard a debit has, so a hold never
   * reserves money the account has not got.
   */
  hold(h: { ref: string; account: string; amount: number; currency: string; ttlMs: number }, key?: string): Promise<Result> {
    const bad =
      invalid(h.amount, h.ref) ?? (Number.isSafeInteger(h.ttlMs) && h.ttlMs > 0 ? null : 'a hold lasts a whole number of milliseconds');
    if (bad) return Promise.resolve(refused('invalid', 400, bad));
    return this.#run(
      [
        [
          'set accounts {held: held + $2} where ext = $1 and status = "open" and kind = "customer" and currency = $5 and balance - held >= $2 require 1',
          'funds',
        ],
        [
          'insert holds {ref: $3, account: $1, currency: $5, amount: $2, captured: 0, state: "held", until: now() + $4, at: now()}',
          'duplicate',
        ],
      ],
      [h.account, h.amount, h.ref, h.ttlMs, h.currency],
      h.ref,
      key,
    );
  }

  /**
   * Takes up to the held amount and pays it to `to`. The hold's state is
   * the guard: only a hold still `held`, and not past `until`, becomes
   * `captured`, so a capture racing a release or the reaper wins or loses
   * whole.
   */
  async capture(c: { hold: string; ref: string; to: string; amount: number; memo?: string }, key?: string): Promise<Result> {
    const bad = invalid(c.amount, c.ref);
    if (bad) return refused('invalid', 400, bad);
    const h = await this.#hold(c.hold);
    if (!h) return refused('not_found', 404, `no hold ${c.hold}`);
    if (c.amount > h.amount) return refused('invalid', 400, 'a capture takes at most what was held');
    return this.#move(
      [
        [
          'set holds {state: "captured", captured: $3} where ref = $10 and account = $1 and amount = $11 and state = "held" and until > now() require 1',
          'hold',
        ],
        [usable(2), 'recipient'],
        ['set accounts {held: held - $11, balance: balance - $3} where ext = $1 and held >= $11 require 1', 'funds'],
        ['set accounts {balance: balance + $3} where ext = $2 require 1', 'recipient'],
      ],
      [h.account, c.to, c.amount, h.currency, c.ref, 'capture', c.memo ?? ''],
      [c.hold, h.amount],
      key,
    );
  }

  /** Gives a hold's money back, once: the state moves from `held`, or nothing does. */
  async release(ref: string, opts: { lapsed?: boolean } = {}): Promise<Result> {
    const h = await this.#hold(ref);
    if (!h) return refused('not_found', 404, `no hold ${ref}`);
    const lapsed = opts.lapsed ? ' and until <= now()' : '';
    return this.#run(
      [
        [`set holds {state: $4} where ref = $1 and account = $2 and amount = $3 and state = "held"${lapsed} require 1`, 'hold'],
        ['set accounts {held: held - $3} where ext = $2 and held >= $3 require 1', 'invalid'],
      ],
      [ref, h.account, h.amount, opts.lapsed ? 'lapsed' : 'released'],
      ref,
    );
  }

  /**
   * Releases every hold past its time, each in a block of its own guarded
   * by `state = "held" and until <= now()`: two reapers, or a reaper and a
   * capture, cannot both give the same money back. Returns the holds this
   * call released.
   */
  async reap(): Promise<string[]> {
    const released: string[] = [];
    for (;;) {
      const due = await this.store.rows<{ ref: string }>('get holds select ref where state = "held" and until <= now() limit 200');
      let moved = 0;
      for (const { ref } of due) {
        if ((await this.release(ref, { lapsed: true })).ok) {
          released.push(ref);
          moved++;
        }
      }
      if (!moved) return released;
    }
  }

  async #hold(ref: string) {
    const [h] = await this.store.rows<{ account: string; amount: number; currency: string }>(
      'get holds select account, amount, currency where ref = $1 limit 1',
      [ref],
    );
    return h ?? null;
  }

  /** Opens a customer account at zero: money comes in only by a deposit. */
  open(a: { ext: string; name: string; holders: string[]; currency: string }, who: string): Promise<Result> {
    if (!/^[a-z0-9_-]{3,64}$/.test(a.ext)) return Promise.resolve(refused('invalid', 400, 'an account id is 3 to 64 of a-z, 0-9, _ and -'));
    if (!/^[A-Z]{3}$/.test(a.currency)) return Promise.resolve(refused('invalid', 400, 'a currency is its three-letter code'));
    return this.#run(
      [
        [
          'insert accounts {ext: $1, name: $2, holders: $3, currency: $4, balance: 0, held: 0, status: "open", kind: "customer", opened: now()}',
          'duplicate',
        ],
        ['insert events {who: $5, what: "opened", account: $1, detail: $4, at: now()}', 'invalid'],
      ],
      [a.ext, a.name, a.holders, a.currency, who],
      a.ext,
    );
  }

  /** Freezes or thaws an account; a frozen one sends and receives nothing. */
  setStatus(ext: string, status: 'open' | 'frozen', who: string, why = ''): Promise<Result> {
    return this.#run(
      [
        ['set accounts {status: $2} where ext = $1 and kind = "customer" and status != $2 require 1', 'source'],
        ['insert events {who: $3, what: $2, account: $1, detail: $4, at: now()}', 'invalid'],
      ],
      [ext, status === 'open' ? 'open' : 'frozen', who, why],
      ext,
    );
  }

  /**
   * A block that moves money: its guards and the two sides, then the two
   * entries and the movement's record. `base` is $1 to $7 in every one --
   * from, to, amount, currency, ref, kind, memo -- $8 and $9 the entries'
   * ids, and `extra` from $10. An entry's id is the movement's ref and its
   * side, which is what the change stream's consumer deduplicates by.
   */
  #move(steps: Step[], base: unknown[], extra: unknown[], key?: string): Promise<Result> {
    const ref = base[4] as string;
    // A refund and a capture name what they come of ($10).
    const of = base[5] === 'refund' || base[5] === 'reversal' || base[5] === 'capture' ? '$10' : 'null';
    return this.#run(
      [
        ...steps,
        [
          'insert journal [{entry: $8, tx: $5, account: $1, currency: $4, amount: 0 - $3, kind: $6, at: now()}, ' +
            '{entry: $9, tx: $5, account: $2, currency: $4, amount: $3, kind: $6, at: now()}]',
          'duplicate',
        ],
        [
          `insert transfers {ref: $5, kind: $6, src: $1, dst: $2, currency: $4, amount: $3, refunded: 0, of: ${of}, memo: $7, at: now()}`,
          'duplicate',
        ],
      ],
      [...base, `${ref}:dr`, `${ref}:cr`, ...extra],
      ref,
      key,
    );
  }

  async #run(steps: Step[], params: unknown[], ref: string, key?: string): Promise<Result> {
    const out: Outcome = await this.store.batch(
      steps.map((s) => s[0]),
      params,
      { key },
    );
    if (out.ok) return { ok: true, ref, replayed: out.replayed, seq: out.seq };
    // A @unique ref taken already: this movement was made before. In
    // process, where there is no Idempotency-Key, that is how a retry
    // learns it landed.
    if (out.status === 409) return refused('duplicate', 409, out.error);
    const reason = out.at !== undefined ? steps[out.at]?.[1] : undefined;
    return refused(reason ?? 'invalid', out.status, out.error);
  }
}

/** The guard that `actor` ($11) holds account `$n`, when there is an actor. */
function holds(actor: string | undefined, n: number): Step[] {
  return actor ? [[`get accounts select ext where ext = $${n} and holders has $11 limit 1 require 1`, 'not_holder']] : [];
}

function refused(reason: Refusal, status: number, error: string): Result {
  return { ok: false, reason, status, error };
}

function invalid(amount: number, ref: string): string | null {
  if (!Number.isSafeInteger(amount) || amount < 1) return 'an amount is a whole number of minor units, at least 1';
  if (!/^[A-Za-z0-9_:-]{4,96}$/.test(ref)) return 'a ref is 4 to 96 of A-Z, a-z, 0-9, _, : and -';
  return null;
}
