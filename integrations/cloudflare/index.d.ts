import type { Fenec } from '@fenecdb/web';

/** The cursor `sql.exec` answers: rows as objects, or as arrays through `raw()`. */
export interface SqlCursor {
  raw(): Iterator<unknown[]>;
  [Symbol.iterator](): Iterator<Record<string, unknown>>;
}

/**
 * What `persist` and `restore` use of a Durable Object's storage
 * (`ctx.storage`). With `sql` -- a SQLite-backed object, the only kind a
 * new account makes -- a generation is a table of its own, a write a row
 * of it; without, the key-value methods hold it in values under 128 KiB.
 */
export interface Storage {
  sql?: { exec(query: string, ...bindings: any[]): SqlCursor };
  transactionSync?<T>(fn: () => T): T;
  /** Resolves once the writes handed over are durable; `persist` waits for it. */
  sync?(): Promise<void>;
  get(key: string): Promise<unknown>;
  put(key: string, value: unknown): Promise<void>;
  delete(keys: string[]): Promise<unknown>;
  list(options: { prefix: string; start?: string; limit?: number }): Promise<Map<string, unknown>>;
}

export interface Options {
  /** Where in the storage the database is kept; `fenec` by default: the table `fenec:<gen>`, or keys under `fenec:`. */
  key?: string;
  /**
   * The largest row or value a part of the image or the writes takes, in
   * bytes: `ROW` with `sql`, `PIECE` without.
   */
  piece?: number;
}

/** The largest row of the image or the writes with `sql`: under the 2 MB a row takes. */
export const ROW: number;

/** The largest value of the image or the writes without `sql`: under a key-value backed object's 128 KiB. */
export const PIECE: number;

/**
 * Writes the database into `storage`: the image the first time, then only
 * the writes since -- a row, with `sql` -- out at once, or with the writes
 * of every request landing while the flush before them is on its way.
 * Resolves once they are durable, to the bytes the flush carrying them
 * wrote, or to 0 when the call had none. Rejects when they were not
 * stored, and from then on: the database is then ahead of its storage, and
 * has to be opened again and restored.
 */
export function persist(fenec: Fenec<any>, storage: Storage, options?: Options): Promise<number>;

/**
 * Writes a new image, graphs included, once the writes kept since the
 * last one reach `min` bytes (64 KiB by default), so a restore takes the
 * graphs rather than link every vector written since. Resolves to the
 * bytes written, 0 when it wrote no image.
 */
export function checkpoint(fenec: Fenec<any>, storage: Storage, options?: Options & { min?: number }): Promise<number>;

/**
 * Loads the database kept under `key` into `fenec`, a row or a value at a
 * time into the module's memory. Resolves to false when there is none.
 */
export function restore(fenec: Fenec<any>, storage: Storage, options?: Pick<Options, 'key'>): Promise<boolean>;
