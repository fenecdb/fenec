import type { ReactNode } from 'react';

/** What the hooks need of a synced database: fenecdb's `FenecSync`. */
export interface LiveSource {
  live(
    query: LiveQuery,
    cb: (rows: any[]) => void,
    opts?: { onError?: (error: unknown) => void },
  ): () => void;
}

/** What the hooks need of a query: fenecdb's `Query`. */
export interface LiveQuery {
  toFenecQL(): [string, unknown[]];
  readonly context?: unknown;
}

/** Makes `db` the synced database `useFenec` and `useLiveQuery` use below. */
export function FenecProvider(props: { db: LiveSource; children?: ReactNode }): ReactNode;

/** The synced database of the nearest `FenecProvider`. */
export function useFenec<DB extends LiveSource = LiveSource>(): DB;

/**
 * The rows of `query`, from the local replica, given again every time they
 * change; `undefined` until the first answer.
 */
export function useLiveQuery<Row = Record<string, unknown>>(
  query: LiveQuery | null | undefined,
): Row[] | undefined;
