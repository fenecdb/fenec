// Where the ledger's statements run: fenec-server over HTTP, or the engine
// in this process (the WebAssembly module of @fenecdb/web). The ledger
// writes every movement as one block of statements sharing one list of
// parameters, so both run the same text.
import type { Fenec } from '@fenecdb/web';

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
 * tenant node) with `token`. `/batch` runs the statements under the write
 * lock as one block; with `key`, as an `Idempotency-Key`, a retry is
 * answered as the first try was and moves nothing twice.
 * `@fenecdb/web/client` has no `/batch` (README, "Gaps"), hence this.
 */
export class HttpStore implements Store {
  constructor(
    readonly base: string,
    readonly token: string,
  ) {}

  async batch(statements: string[], params: unknown[], opts: { key?: string } = {}): Promise<Outcome> {
    const headers: Record<string, string> = {
      'content-type': 'application/x-ndjson',
      authorization: `Bearer ${this.token}`,
    };
    if (opts.key) headers['idempotency-key'] = opts.key;
    const body = statements.map((query) => JSON.stringify({ query, params })).join('\n');
    const res = await fetch(`${this.base}/batch`, { method: 'POST', headers, body });
    const json = await parsed(res);
    if (res.ok) {
      const seq = Number(res.headers.get('fenec-seq') ?? NaN);
      return {
        ok: true,
        results: json.results ?? [],
        replayed: res.headers.get('idempotent-replayed') === 'true',
        seq: Number.isFinite(seq) ? seq : undefined,
      };
    }
    return { ok: false, status: res.status, error: json.error ?? `HTTP ${res.status}`, at: json.at };
  }

  async rows<T = Record<string, unknown>>(query: string, params: unknown[] = []): Promise<T[]> {
    const res = await fetch(`${this.base}/query`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', authorization: `Bearer ${this.token}` },
      body: JSON.stringify({ query, params }),
    });
    const json = await parsed(res);
    if (!res.ok) throw new StoreError(json.error ?? `HTTP ${res.status}`, res.status);
    return (Array.isArray(json) ? json : (json.rows ?? [])) as T[];
  }

  // A /batch of reads runs under the write lock like any batch: no write
  // lands between them, so they read one state, and `Fenec-Seq` names it.
  async snapshot(queries: string[]) {
    const out = await this.batch(queries, []);
    if (!out.ok) throw new StoreError(out.error, out.status);
    const results = out.results.map((r) => (r as { rows?: Record<string, unknown>[] }).rows ?? []);
    return { results, seq: out.seq };
  }
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
async function parsed(res: Response): Promise<any> {
  const text = await res.text();
  try {
    return text ? JSON.parse(text) : {};
  } catch {
    throw new StoreError(`fenec-server did not answer JSON (${res.status}): ${text.slice(0, 200)}`, res.status);
  }
}

/**
 * The engine in this process. `run` of several statements is one block,
 * put back whole when one fails, as a `/batch` is. Two things a `/batch`
 * has the module's `run` has not (README, "Gaps"):
 *
 * - it does not say which statement stopped it, so on a failure the block
 *   is run again a prefix at a time, each ended by a statement that always
 *   fails, until the failure is another's: nothing lands, and it is
 *   synchronous, so no other client's block comes between;
 * - there is no `Idempotency-Key`; the ledger's `@unique` refs are what
 *   keep a retry from landing twice in process.
 */
export class LocalStore implements Store {
  constructor(readonly db: Fenec) {}

  async batch(statements: string[], params: unknown[]): Promise<Outcome> {
    try {
      this.db.run(statements.join('\n'), params);
      return { ok: true, results: [], replayed: false, seq: this.db.changeSeq };
    } catch (e) {
      const error = (e as Error).message;
      return { ok: false, status: statusOf(error), error, at: this.#stoppedAt(statements, params, error) };
    }
  }

  #stoppedAt(statements: string[], params: unknown[], error: string): number | undefined {
    const probe = 'get events where who = "\u0000probe" limit 1 require 1';
    for (let k = 0; k < statements.length; k++) {
      try {
        this.db.run([...statements.slice(0, k + 1), probe].join('\n'), params);
      } catch (e) {
        if ((e as Error).message !== error) continue; // the probe's, or another
        return k;
      }
    }
    return undefined;
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
