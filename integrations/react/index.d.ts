import type { ReactNode } from 'react';

/**
 * What the hooks need of a database: fenecdb's `Fenec`, a database in the
 * page, or `FenecSync`, a replica synced from a server.
 */
export interface LiveSource {
  live(
    query: LiveQuery | string | [string, unknown[]],
    cb: (rows: any[]) => void,
    opts?: { onError?: (error: unknown) => void; collections?: string[] },
  ): () => void;
}

/** What the hooks need of a query: fenecdb's `Query`. */
export interface LiveQuery {
  toFenecQL(): [string, unknown[]];
  readonly context?: unknown;
}

/** Makes `db` the database `useFenec` and `useLiveQuery` use below. */
export function FenecProvider(props: { db: LiveSource; children?: ReactNode }): ReactNode;

/** The database of the nearest `FenecProvider`. */
export function useFenec<DB extends LiveSource = LiveSource>(): DB;

/**
 * The rows of `query`, from the database in the page, given again every
 * time they change; `undefined` until the first answer. A FenecQL text or
 * `[text, params]` runs on the provider's database, and names what it reads
 * with `collections` (without them, every write runs it again).
 */
export function useLiveQuery<Row = Record<string, unknown>>(
  query: LiveQuery | string | [string, unknown[]] | null | undefined,
  opts?: { collections?: string[] },
): Row[] | undefined;
