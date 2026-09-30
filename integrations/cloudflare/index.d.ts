import type { Fenec } from '@fenecdb/web';

/**
 * What `persist` and `restore` use of a Durable Object's storage
 * (`ctx.storage`): its key-value API.
 */
export interface Storage {
  get(key: string): Promise<unknown>;
  put(key: string, value: unknown): Promise<void>;
  delete(keys: string[]): Promise<unknown>;
  list(options: { prefix: string }): Promise<Map<string, unknown>>;
}

export interface Options {
  /** Where in the storage the database is kept; `fenec` by default. */
  key?: string;
  /**
   * The largest piece a value holds, in bytes: under 128 KiB by default,
   * which a key-value backed object takes; a SQLite-backed one takes up to
   * 2 MB with the key.
   */
  piece?: number;
}

/** The default piece: under a key-value backed object's 128 KiB. */
export const PIECE: number;

/**
 * Writes the database into `storage`: the image the first time, then only
 * the writes since the last call. Resolves to the bytes written.
 */
export function persist(fenec: Fenec<any>, storage: Storage, options?: Options): Promise<number>;

/**
 * Loads the database kept under `key` into `fenec`. Resolves to false when
 * there is none.
 */
export function restore(fenec: Fenec<any>, storage: Storage, options?: Pick<Options, 'key'>): Promise<boolean>;
