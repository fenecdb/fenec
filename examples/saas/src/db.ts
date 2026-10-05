// One tenant, reached through the router with one token, over
// `@fenecdb/web/client`. A block of statements is `POST /batch`: they land
// whole or not at all, a `require` that is not met stops it with 412 and
// the place of the statement (`at`), and an idempotency key makes a retry
// answer as the first try did. A refusal comes back as an Outcome rather
// than an exception, since most of them are answers: a used token, a taken
// slug, a rate limit.
import { connect, FenecError, type FenecHttp } from '@fenecdb/web/client';

export type Outcome =
  | { ok: true; results: { rows?: Record<string, unknown>[]; affected?: number }[]; replayed: boolean; seq?: number }
  | { ok: false; status: number; error: string; at?: number };

export class DbError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

export class Db {
  readonly #http: FenecHttp;

  constructor(
    readonly base: string,
    readonly token: string,
  ) {
    this.#http = connect(base, { token });
  }

  async batch(statements: [string, unknown[]][], opts: { key?: string } = {}): Promise<Outcome> {
    try {
      const r = await this.#http.batch(statements, { idempotencyKey: opts.key });
      return { ok: true, results: r.results as never, replayed: r.replayed, seq: r.seq ?? undefined };
    } catch (e) {
      if (!(e instanceof FenecError) || e.status === undefined) throw e;
      return { ok: false, status: e.status, error: e.message, at: e.at };
    }
  }

  async rows<T = Record<string, unknown>>(query: string, params: unknown[] = []): Promise<T[]> {
    try {
      return (await this.#http.rows(query, params)) as T[];
    } catch (e) {
      if (e instanceof FenecError && e.status !== undefined) throw new DbError(e.message, e.status);
      throw e;
    }
  }

  async one<T = Record<string, unknown>>(query: string, params: unknown[] = []): Promise<T | undefined> {
    return (await this.rows<T>(query, params))[0];
  }
}
