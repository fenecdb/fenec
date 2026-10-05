// Where the ledger's statements run: fenec-server over HTTP, or the engine
// in this process (the WebAssembly module of @fenecdb/web). The ledger
// writes every movement as one block of statements sharing one list of
// parameters, so both run the same text.
import type { Fenec } from '@fenecdb/web';
import { connect, FenecError, type FenecHttp } from '@fenecdb/web/client';

export type Outcome =
  | { ok: true; results: unknown[]; replayed: boolean; seq?: number }
  /** `at`: the statement that stopped the block, from 0. */
  | { ok: false; status: number; error: string; at?: number };

export interface Store {
  /** The statements as one block: all of them land, or none. */
  batch(statements: string[], params: unknown[], opts?: { key?: string }): Promise<Outcome>;
  /** One read, its rows. */
  rows<T = Record<string, unknown>>(query: string, params?: unknown[]): Promise<T[]>;
  /** Several reads that see one state of the database, and the change it stood at. */
  snapshot(queries: string[]): Promise<{ results: Record<string, unknown>[][]; seq?: number }>;
}

export class StoreError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

/**
 * fenec-server, reached at `base` (`http://host:port/t/<tenant>` on a
 * tenant node) with `token`, through `@fenecdb/web/client`. Its `batch` is
 * `POST /batch`: the statements under the write lock as one block, and
 * with `idempotencyKey` a retry is answered as the first try was and moves
 * nothing twice. A refusal comes back as a `FenecError` with its `status`
 * and the statement it stopped at (`at`), which the ledger answers rather
 * than throws.
 */
export class HttpStore implements Store {
  readonly #db: FenecHttp;

  constructor(
    readonly base: string,
    readonly token: string,
  ) {
    this.#db = connect(base, { token });
  }

  async batch(statements: string[], params: unknown[], opts: { key?: string } = {}): Promise<Outcome> {
    try {
      const r = await this.#db.batch(
        statements.map((q) => [q, params] as const),
        { idempotencyKey: opts.key },
      );
      return { ok: true, results: r.results, replayed: r.replayed, seq: r.seq ?? undefined };
    } catch (e) {
      if (!(e instanceof FenecError) || e.status === undefined) throw e;
      return { ok: false, status: e.status, error: e.message, at: e.at };
    }
  }

  async rows<T = Record<string, unknown>>(query: string, params: unknown[] = []): Promise<T[]> {
    try {
      return (await this.#db.rows(query, params)) as T[];
    } catch (e) {
      if (e instanceof FenecError && e.status !== undefined) throw new StoreError(e.message, e.status);
      throw e;
    }
  }

  // A /batch of reads alone runs under one read lock: no write lands
  // between them, so they read one state, and `Fenec-Seq` names it. Other
  // reads go on beside it; a write waits for it, as for any read.
  async snapshot(queries: string[]) {
    const out = await this.batch(queries, []);
    if (!out.ok) throw new StoreError(out.error, out.status);
    const results = out.results.map((r) => (r as { rows?: Record<string, unknown>[] }).rows ?? []);
    return { results, seq: out.seq };
  }
}

/**
 * The engine in this process. `run` of several statements is one block,
 * put back whole when one fails, as a `/batch` is, and its error names the
 * statement that stopped it (`at`), as a `/batch`'s does. There is no
 * `Idempotency-Key`; the ledger's `@unique` refs are what keep a retry
 * from landing twice in process.
 */
export class LocalStore implements Store {
  constructor(readonly db: Fenec) {}

  async batch(statements: string[], params: unknown[]): Promise<Outcome> {
    try {
      this.db.run(statements.join('\n'), params);
      return { ok: true, results: [], replayed: false, seq: this.db.changeSeq };
    } catch (e) {
      if (!(e instanceof FenecError)) throw e;
      const error = e.message;
      // A block of one statement names no place: it is the first.
      return { ok: false, status: statusOf(error), error, at: e.at ?? 0 };
    }
  }

  async rows<T = Record<string, unknown>>(query: string, params: unknown[] = []): Promise<T[]> {
    try {
      const r = this.db.run(query, params) as { rows?: T[] };
      return r.rows ?? [];
    } catch (e) {
      const m = (e as Error).message;
      throw new StoreError(m, statusOf(m));
    }
  }

  // The module answers one statement at a time, synchronously: nothing
  // runs between these reads.
  async snapshot(queries: string[]) {
    const results = queries.map((q) => (this.db.run(q) as { rows?: Record<string, unknown>[] }).rows ?? []);
    return { results, seq: this.db.changeSeq };
  }
}

/** The status fenec-server would answer the error with. */
function statusOf(message: string): number {
  if (message.startsWith('unmet:')) return 412;
  if (/is unique, and the value is taken/.test(message)) return 409;
  return 400;
}
