// Type declarations for the fenecdb client.
//
// The schema does not live here, it lives in the database: `fenec types
// data.fenec` generates `FenecSchema` from the running file. Unlike Drizzle the
// schema is not kept in two places, so the two cannot drift apart.
//
//   fenec types data.fenec > web/fenec-schema.d.ts
//
//   import { Fenec } from './fenec.js';
//   import type { FenecSchema } from './fenec-schema.js';
//   const db = await Fenec.open<FenecSchema>('./fenec.wasm');
//   const rows = await db.from('articles').select('title').rows();
//        // rows: { title: string }[]

/** A `timestamp` field. ISO-8601 text when read; Date/number on write. */
export type Timestamp = string & { readonly __fenec: 'timestamp' };
/** A `vector<N>` field. */
export type Vector = number[] & { readonly __fenec: 'vector' };
/** A `bytes` field: an array of bytes in JSON. */
export type Bytes = number[] & { readonly __fenec: 'bytes' };

/** A collection's read shape; `fenec types` generates these. */
export type Fields = Record<string, unknown>;
export type Schema = Record<string, Fields>;

// The schema constraint is `Record<keyof S, Fields>` rather than
// `Record<string, Fields>`: a hand-written `interface` has no implicit index
// signature and the latter would reject it. `fenec types` emits a `type`, but
// both have to work.
type AnySchema<S> = Record<keyof S, Fields>;

/** A row as read: the fields plus the automatic `id`. */
export type Row<F extends Fields> = F & { id: number };

/**
 * The child side of a `lookup`. `on` is the child's field; the parent's key
 * is `id` unless `parentKey` names another. `order` takes `[field, dir]`
 * pairs, or a bare field name for one ascending key.
 *
 * With no `C` every name is a plain `string`, which is the honest default:
 * the builder carries one collection's fields, so the child's are only
 * known when the caller names them. `parentKey` stays `string` for the same
 * reason from the other side.
 */
export interface LookupOptions<C extends Fields = Fields> {
  on: keyof Row<C> & string;
  parentKey?: string;
  /**
   * Drop a parent that no child matches — "products that have a five-star
   * review" rather than "products, with their five-star reviews". Tested
   * before `limit`, so the page still comes back full.
   */
  required?: boolean;
  select?: ChildKey<C> | ChildKey<C>[];
  where?: Where<C> | Cond<C>;
  order?: ChildKey<C> | Array<ChildKey<C> | [ChildKey<C>, ('asc' | 'desc')?]>;
  limit?: number;
  offset?: number;
}

type ChildKey<C extends Fields> = keyof Row<C> & string;

/** The widened type accepted in write position. */
export type Writable<T> = T extends Timestamp
  ? Timestamp | string | number | Date
  : T extends Vector
    ? number[] | Float32Array
    : T extends Bytes
      ? number[] | Uint8Array | string
      : T;

type Elem<T> = T extends readonly (infer U)[] ? U : never;

/**
 * Fields of type `vector<N>` -- the only ones `near` accepts. An optional
 * field is generated as `Vector | null`, so `null` is peeled off first.
 */
export type VectorKey<F extends Fields> = {
  [K in keyof F]: NonNullable<F[K]> extends Vector ? K : never;
}[keyof F] &
  string;

/**
 * Fields of type `text` -- the only ones `match` accepts. Whether the field
 * actually carries a `@text` index is a schema question the engine answers;
 * the type only rules out the fields that could never have one.
 */
export type TextKey<F extends Fields> = {
  [K in keyof F]: NonNullable<F[K]> extends string ? K : never;
}[keyof F] &
  string;

export type Op =
  | '=' | '!=' | '<' | '<=' | '>' | '>=' | '~' | 'has' | 'in'
  | 'eq' | 'ne' | 'neq' | 'lt' | 'lte' | 'le' | 'gt' | 'gte' | 'ge'
  | 'like' | 'contains';

/** The operator object inside `{ year: { gte: 2024 } }`. */
export interface Spec<T> {
  '='?: Writable<T> | null;
  eq?: Writable<T> | null;
  '!='?: Writable<T> | null;
  ne?: Writable<T> | null;
  neq?: Writable<T> | null;
  '<'?: Writable<T>;
  lt?: Writable<T>;
  '<='?: Writable<T>;
  lte?: Writable<T>;
  le?: Writable<T>;
  '>'?: Writable<T>;
  gt?: Writable<T>;
  '>='?: Writable<T>;
  gte?: Writable<T>;
  ge?: Writable<T>;
  '~'?: string;
  like?: string;
  contains?: string;
  has?: Elem<T>;
  in?: Writable<T>[];
  not?: Writable<T> | null | Spec<T>;
}

// `F` is not inferred: the object inside `or({...})` is checked against the
// collection `where` expects -- not against the first argument.
type Hold<T> = [T][T extends any ? 0 : never];

declare const cond: unique symbol;

/**
 * The output of `or()` / `not()` / `raw()`. Its contents are opaque: the
 * brand is a `unique symbol`, so it does not interfere with field-name
 * completion. `F` stays contravariant, which means a condition built
 * without a context (`Cond<Fields>`) is valid for every collection, but not
 * the other way round.
 */
export interface Cond<F extends Fields = Fields> {
  readonly [cond]: (f: F) => void;
}

/** A field-value object: every condition is joined with `and`. */
export type Where<F extends Fields> = {
  [K in keyof Row<F>]?: Writable<Row<F>[K]> | null | Spec<Row<F>[K]>;
};

export function or<F extends Fields = Fields>(...conds: (Where<Hold<F>> | Cond<Hold<F>>)[]): Cond<F>;
export function and<F extends Fields = Fields>(...conds: (Where<Hold<F>> | Cond<Hold<F>>)[]): Cond<F>;
export function not<F extends Fields = Fields>(cond: Where<Hold<F>> | Cond<Hold<F>>): Cond<F>;

/**
 * Escape hatch for everything the builder cannot express; `?` placeholders
 * are bound to parameters in order.
 *
 *   .where(raw('cosine(embed, ?) > ?', vec, 0.5))
 */
export function raw<F extends Fields = Fields>(sql: string, ...params: unknown[]): Cond<F>;

/** Executor: `(sql, params)` -> response. A `Fenec` instance also works. */
export type Exec = (sql: string, params: unknown[]) => unknown;

export declare class FenecError extends Error {}

/**
 * Immutable query builder. `F` is the collection's fields, `P` the result
 * of the projection so far.
 */
export declare class Query<F extends Fields = Fields, P = Row<F>> {
  /** Binds the query to an executor (wasm, HTTP, fenec-pg). */
  bind(exec: Exec | { run(sql: string, params: unknown[]): unknown }): Query<F, P>;

  select<K extends keyof Row<F> & string>(
    ...cols: (K | K[])[]
  ): Query<F, Pick<Row<F>, K>>;
  select(): Query<F, Row<F>>;

  where(cond: Where<F> | Cond<F>): Query<F, P>;
  where<K extends keyof Row<F> & string>(
    field: K,
    value: Writable<Row<F>[K]> | null | Spec<Row<F>[K]>,
  ): Query<F, P>;
  where<K extends keyof Row<F> & string>(
    field: K,
    op: 'in',
    values: Writable<Row<F>[K]>[],
  ): Query<F, P>;
  where<K extends keyof Row<F> & string>(
    field: K,
    op: 'has',
    value: Elem<Row<F>[K]>,
  ): Query<F, P>;
  where<K extends keyof Row<F> & string>(
    field: K,
    op: Op,
    value: Writable<Row<F>[K]> | null,
  ): Query<F, P>;

  orWhere(cond: Where<F> | Cond<F>): Query<F, P>;
  orWhere<K extends keyof Row<F> & string>(
    field: K,
    value: Writable<Row<F>[K]> | null | Spec<Row<F>[K]>,
  ): Query<F, P>;
  orWhere<K extends keyof Row<F> & string>(
    field: K,
    op: Op,
    value: unknown,
  ): Query<F, P>;

  /** Vector search; `_score` is added to the result. */
  near(
    field: VectorKey<F>,
    vector: number[] | Float32Array,
    opts?: { ef?: number; exact?: boolean },
  ): Query<F, P & { _score: number }>;

  /** Full-text search over a `@text` index; `_score` is added to the result. */
  match(field: TextKey<F>, query: string): Query<F, P & { _score: number }>;

  /**
   * Reorders what `match` found by exact vector distance. Requires `match`,
   * but not an `@hnsw` index: the vectors are read out of the store.
   */
  rerank(
    field: VectorKey<F>,
    vector: number[] | Float32Array,
    opts?: { candidates?: number },
  ): Query<F, P & { _score: number }>;

  /**
   * Attaches the children of another collection to each row.
   *
   * Everything in `opts` binds to the looked-up collection: `limit` there
   * counts children **per parent**, which is the shape a join cannot
   * express. The clause is terminal in FenecQL, so it is emitted last
   * whatever order the builder was called in.
   *
   * The child's field type cannot be reached from `Query<F>` -- the builder
   * carries one collection's fields, not the schema -- so it defaults to a
   * loose row. Pass it to get the tight one:
   *
   *     q.lookup<'reviews', FenecSchema['reviews']>('reviews', { on: 'product_id' })
   */
  lookup<N extends string, C extends Fields = Fields>(
    name: N,
    opts: LookupOptions<C>,
  ): Query<F, P & { [K in N]: Row<C>[] }>;

  /** Successive calls add a sort key (the second decides when the first ties). */
  order(field: keyof Row<F> & string, dir?: 'asc' | 'desc'): Query<F, P>;
  limit(n: number): Query<F, P>;
  offset(n: number): Query<F, P>;

  /** The generated FenecQL and its parameters -- inspectable before running. */
  toFenecQL(): [sql: string, params: unknown[]];

  /** The text of the write statements, without running them. The write side of `toFenecQL`. */
  toInsert(docs: Insert<F> | Insert<F>[]): [sql: string, params: unknown[]];
  toUpdate(patch: Insert<F>, opts?: { all?: boolean }): [sql: string, params: unknown[]];
  toDelete(opts?: { all?: boolean }): [sql: string, params: unknown[]];

  /** The query's collection. */
  readonly collection: string;
  /** The opaque context carried by `bind` (for subclasses). */
  readonly context: unknown;
  /** The same body as a plain `Query`: bypasses subclass behaviour. */
  plain(): Query<F, P>;

  run(): Promise<{ columns: string[]; rows: P[] }>;
  rows(): Promise<P[]>;
  first(): Promise<P | null>;
  /** Number of matching rows (`get ... count`); rows are not decoded. */
  count(): Promise<number>;

  insert(docs: Insert<F> | Insert<F>[]): Promise<number>;
  update(patch: Insert<F>, opts?: { all?: boolean }): Promise<number>;
  delete(opts?: { all?: boolean }): Promise<number>;
}

/** A written document: every field optional; passing `id` makes it an upsert. */
export type Insert<F extends Fields> = {
  [K in keyof Row<F>]?: Writable<Row<F>[K]> | null;
};

/**
 * Unbound builder: it only generates text, and `bind()` runs it. The
 * collection name is free here; the schema type is given per query as the
 * field type, or for all of them with `TypedFrom`.
 *
 *   from<FenecSchema['articles']>('articles').where('year', 2024)
 */
export function from<F extends Fields = Fields>(name: string): Query<F>;

/**
 * Binds `from` to a schema -- purely a type, the same function at runtime.
 * Completes collection and field names just like `db.from`.
 *
 *   const from: TypedFrom<FenecSchema> = fenecFrom;
 *   from('articles').where('year', '>=', 2024)
 */
export type TypedFrom<S extends AnySchema<S>> = <K extends keyof S & string>(
  name: K,
) => Query<S[K]>;

/** Schema information from the `collections` output. */
export interface SchemaInfo {
  name: string;
  fields: { name: string; type: string; index?: string }[];
}

/** Remote endpoint options. Without `fetch`, `globalThis.fetch` is used. */
export interface HttpOptions {
  token?: string;
  fetch?: typeof globalThis.fetch;
}

/**
 * Remote fenecdb HTTP endpoint (`fenec-pg --http`). The builder generates the
 * same FenecQL text; only the transport differs.
 */
export declare class FenecHttp<S extends AnySchema<S> = Schema> {
  constructor(url: string, opts?: HttpOptions);
  from<K extends keyof S & string>(name: K): Query<S[K]>;
  run(sql: string, params?: unknown[]): Promise<any>;
  rows(sql: string, params?: unknown[]): Promise<any[]>;
  schemas(): Promise<{ collections?: SchemaInfo[] } | SchemaInfo[]>;
}

export function connect<S extends AnySchema<S> = Schema>(
  url: string,
  opts?: HttpOptions,
): FenecHttp<S>;

export declare class Fenec<S extends AnySchema<S> = Schema> {
  /** Connects to a remote HTTP endpoint. */
  static connect<S extends AnySchema<S> = Schema>(
    url: string,
    opts?: HttpOptions,
  ): FenecHttp<S>;

  /**
   * Loads the WASM module. On Node the file bytes are passed directly:
   * `Fenec.open(await readFile('fenec.wasm'))`.
   */
  static open<S extends AnySchema<S> = Schema>(
    src?: string | BufferSource,
  ): Promise<Fenec<S>>;

  readonly version: string;

  /** Query builder. */
  from<K extends keyof S & string>(name: K): Query<S[K]>;

  /** Raw FenecQL -- synchronous. */
  run(sql: string, params?: unknown[]): any;
  rows(sql: string, params?: unknown[]): any[];

  schemas(): SchemaInfo[];
  stats(): unknown;
  snapshot(): Uint8Array;
  load(bytes: Uint8Array): void;
  close(): void;

  /**
   * What changed since `since`. When `collections === null` the cursor has
   * fallen behind the change ring: treat everything as stale.
   */
  changes(since?: number): { seq: number; horizon: number; collections: string[] | null };
  /** Current value of the change counter. */
  readonly changeSeq: number;
  /** Entry count of the change ring. */
  setChangeCapacity(n: number): void;
}

/** Writes a snapshot into IndexedDB; returns the byte count. */
export function persist(fenec: Fenec<any>, key?: string): Promise<number>;
/** Restores from IndexedDB; `false` when there is no record. */
export function restore(fenec: Fenec<any>, key?: string): Promise<boolean>;
/** Free-form state (cursors) -- next to the image, under a separate key. */
export function putState(key: string, value: unknown): Promise<void>;
export function getState(key: string): Promise<unknown>;

// -------------------------------------------------------------------- sync

/**
 * A shape: the tracked subset of a collection.
 *
 * `where` is deliberately narrow: it is translated into the
 * `?field=op.value` form, so there are no `or` groups and no function
 * calls. A shape is a subset definition; once it gets complicated, a
 * separate collection on the server is the right answer.
 */
export interface Shape<F extends Fields = Fields> {
  collection: string;
  /** Filter applied server side. */
  where?: Where<F>;
  /** Fields to ship; `id` is always added. */
  select?: (keyof Row<F> & string)[];
  /**
   * The business key field (`text @hash` recommended). Reconciling an
   * optimistic insert with the server id depends on it: **without a key an
   * insert is not applied optimistically**, the row arrives over the
   * subscription.
   */
  key?: keyof F & string;
}

export interface SyncOptions<S extends AnySchema<S> = Schema> {
  /** Server root (`fenec-pg --http`). */
  url: string;
  shapes: Shape<any>[];
  /** An existing local database; otherwise opened from the `wasm` path. */
  local?: Fenec<S>;
  wasm?: string | BufferSource;
  token?: string;
  fetch?: typeof globalThis.fetch;
  /** IndexedDB key: the image **and the cursors** are stored. */
  persist?: string;
  /** `false` turns off multi-tab leader election. */
  leader?: 'auto' | false;
  /** Lock manager (defaults to `navigator.locks`). */
  locks?: { request(name: string, opts: unknown, fn: () => unknown): Promise<unknown> };
  onError?: (e: unknown) => void;
}

export interface ShapeStatus {
  collection: string;
  cursor: number;
  seeded: boolean;
  connected: boolean;
  /** Number of optimistic rows awaiting server confirmation. */
  pending: number;
  error: string | null;
  leader: boolean;
}

/** Batch context: the same surface as `db`, but writes are accumulated. */
export interface Batch<S extends AnySchema<S> = Schema> {
  from<K extends keyof S & string>(name: K): Query<S[K]>;
}

/**
 * Local replica + server connection. Reads are local (no network), writes
 * go local first and then to the server, feedback arrives over the subscription.
 */
export declare class FenecSync<S extends AnySchema<S> = Schema> {
  /** The local database; for raw FenecQL. */
  readonly local: Fenec<S>;
  /** The remote endpoint; for queries outside the shapes. */
  readonly remote: FenecHttp<S>;

  /** Resolves once the first seed of every shape has landed. */
  ready(): Promise<void>;
  status(): ShapeStatus[];

  /** Reads local, writes optimistic. A collection without a shape is rejected. */
  from<K extends keyof S & string>(name: K): Query<S[K]>;

  /**
   * Live query: re-run after every local change.
   * Returns: the function that ends the subscription.
   */
  live<F extends Fields, P>(
    query: Query<F, P>,
    cb: (rows: P[]) => void,
    opts?: { onError?: (e: unknown) => void },
  ): () => void;

  /**
   * Sends several writes in a single round trip. **Not a transaction**:
   * the local side is rolled back exactly, the server side cannot be.
   */
  batch(fn: (t: Batch<S>) => Promise<void>): Promise<number>;

  /** Finishes any pending live-query runs (for tests). */
  flush(): Promise<void>;
  /** Closes the subscriptions; the local database stays open. */
  close(): void;
}

/** Opens the local replica and starts the subscriptions. */
export function sync<S extends AnySchema<S> = Schema>(
  opts: SyncOptions<S>,
): Promise<FenecSync<S>>;
