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
  /** The object's alarm: read once by `schedule` and `maintain`, set when maintenance comes due. */
  getAlarm?(): Promise<number | null>;
  setAlarm?(at: number): Promise<void>;
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

/**
 * Lets the database go: nothing it holds is stored after this, and every
 * `persist`, `checkpoint` and `maintain` on it rejects. Resolves once a
 * flush under way has settled, so a database restored after it reads all
 * this one stored and is the only one writing: call it before opening the
 * database again. A rejection of those calls on a failed or released
 * database likewise comes once no flush of it is under way.
 */
export function release(fenec: Fenec<any>): Promise<void>;

export interface MaintenanceOptions extends Options {
  /** How long past its time (`@ttl`) a row stays for a reader of `expired()` before it is deleted: 60 000 ms. */
  grace?: number;
  /** The least time between two sweeps' alarms, each two rows written: 60 000 ms. */
  every?: number;
  /** Rows a `maintain` deletes at most, the rest left to the next alarm: 10 000. */
  sweep?: number;
  /** Bytes the writes since the image reach before an image is worth writing: 64 KiB, as `checkpoint`'s. */
  min?: number;
}

export interface Maintained {
  /** Rows deleted, past their time by `grace`. */
  swept: number;
  /** Whether more are past their time than `sweep` let go: the alarm is set again for now. */
  more: boolean;
  /** The bytes of the image written, 0 when none was. */
  image: number;
  /**
   * Whether the module holds enough dead records to be worth opening
   * again: storage holds an image without them, and a new `Fenec`
   * restored from it gives them back. Asked once a database.
   */
  reload: boolean;
  /** The alarm set for what comes due next, or null when nothing will, or it could not be set. */
  next: number | null;
  /**
   * What kept the alarm from being set again, or null. No failure of the
   * database: what was done is stored, and a handler can throw it for
   * Cloudflare to try the alarm again, keeping the database.
   */
  error: unknown;
}

/**
 * When `maintain` next has work, in milliseconds since the epoch: now when
 * an image or a reload is owed, else when the first row past its time is
 * `grace` past it (no sooner than `every` after the last sweep); null when
 * nothing will be.
 */
export function due(fenec: Fenec<any>, storage: Storage, options?: MaintenanceOptions): number | null;

/**
 * Sets the object's alarm once `maintain` comes due, `quiet` after now at
 * the soonest (5 000 ms), unless one is set no later than `every` after
 * that: `setAlarm` is a row written. Resolves to the alarm set, or null
 * when nothing is due. Call it after a write. Called, a request folds the
 * writes into an image only once they outgrow the whole image, where it
 * did at half: `maintain` writes that image.
 */
export function schedule(fenec: Fenec<any>, storage: Storage, options?: MaintenanceOptions & { quiet?: number }): Promise<number | null>;

/**
 * An alarm's work: deletes the rows past their time by `grace`, `sweep` at
 * most; writes a new image once the writes since the last reach half of
 * it (and `min`), or half of what storage holds is dead (and 1 MiB);
 * stores both; and sets the alarm again for what comes due next. Rejects
 * as `persist` does when storage fails, and the database must then be
 * opened again (`release`); an alarm it could not set is `error`, not a
 * rejection.
 */
export function maintain(fenec: Fenec<any>, storage: Storage, options?: MaintenanceOptions): Promise<Maintained>;
