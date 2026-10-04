// Type declarations for the fenecdb client.
//
// A database's schema reaches the types two ways, side by side:
//
// - Declared in code, Drizzle's way (`@fenecdb/web/schema`): tables written
//   in TypeScript, handed to `Fenec.open`, which checks the database against
//   them at every open -- what only adds is made, what would lose data or
//   could mean two things is refused, listed with how to resolve it. Two
//   places hold the schema, and the check is what keeps them one: they cannot
//   drift apart unseen. For an app that makes its own database.
//
//     import { fenecTable, text, boolean } from './schema.js';
//     const todos = fenecTable('todos', { title: text().notNull(), done: boolean() });
//     const db = await Fenec.open('./fenec.wasm', { schema: { todos } });
//     const rows = await db.from(todos).select('title').rows();   // { title: string }[]
//
// - Read out of a database: `fenec types data.fenec` generates `FenecSchema`
//   from the file a server runs, which owns its schema. For an app over an
//   existing database. `fenec types --schema` writes the tables instead, for
//   a project that moves to the first way.
//
//     import type { FenecSchema } from './fenec-schema.js';
//     const db = await Fenec.open<FenecSchema>('./fenec.wasm');

/** A `timestamp` field. ISO-8601 text when read; Date/number on write. */
export type Timestamp = string & { readonly __fenec: 'timestamp' };
/** A `vector<N>` field. */
export type Vector = number[] & { readonly __fenec: 'vector' };
/** A `sparse<N>` field: pgvector's text form, `{1:0.5,3:0.25}/N`, indices from 1. */
export type Sparse = string & { readonly __fenec: 'sparse' };
/** A `bytes` field: an array of bytes in JSON. */
export type Bytes = number[] & { readonly __fenec: 'bytes' };
/** A `json` field: any value JSON holds. A path reads into it, `'meta.lang'`. */
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

/**
 * A path into one of `F`'s json fields, `'meta.lang'` or
 * `'meta.source.rank'`: what `where`, `select` and `order` take beside a
 * field, reading a `Json` value -- `null` where the path leads nowhere.
 */
export type JsonPath<F extends Fields> = {
  [K in keyof F & string]: { [key: string]: Json } extends NonNullable<F[K]>
    ? `${K}.${string}`
    : never;
}[keyof F & string];

/** A collection's read shape; `fenec types` generates these. */
export type Fields = Record<string, unknown>;
export type Schema = Record<string, Fields>;

/**
 * A table declared in code (`fenecTable`, `@fenecdb/web/schema`), as the
 * client takes one wherever it takes a collection's name: `db.from(todos)`,
 * `lookup(reviews, ...)`, a shape's `collection`.
 */
export interface TableRef<N extends string = string, F extends Fields = Fields> {
  readonly $name: N;
  /** The fields as read, a type alone: what `fenec types` makes of the collection. */
  readonly $fields: F;
}

/** A schema's tables, by any key: `{ todos, reviews }`. */
export type Tables = Record<string, TableRef>;

/** The schema the client is typed by, of tables declared in code: each table's fields by its name. */
export type SchemaOf<T extends Tables> = Typed<{ [K in keyof T as T[K]['$name']]: T[K]['$fields'] }>;
/** A schema the client takes: what a mapped type over generic tables cannot be shown to be. */
type Typed<S> = S extends AnySchema<S> ? S : never;

/** A relation of `defineRelations`, which `lookup` names: the table it reaches. */
export interface RelationRef<T extends TableRef = TableRef> {
  readonly $target: T;
}

/** What `defineRelations` makes: by table, its relations by name. */
export type Relations = Record<string, Record<string, RelationRef>>;

/**
 * A migration: FenecQL run once, in order, and recorded in `_migrations`
 * -- or a field rebuilt as the code declares it (`rebuild`).
 */
export type Migration = string | { rebuild: { collection: string; field: string } };

/** What every SDK's declarations compile to (`describe`): versioned JSON. */
export interface SchemaDescription {
  format: 1;
  /** The collections as JSON; or, the browser module's way, `fenecql`. */
  collections?: CollectionDescription[];
  /** The collections as FenecQL text, which the browser module reads. */
  fenecql?: string;
  migrations?: readonly Migration[];
}
export interface CollectionDescription {
  name: string;
  fields: FieldDescription[];
}
export interface FieldDescription {
  name: string;
  /** As FenecQL spells it: `text`, `int`, `vector<768, f16>`, `[text]` ... */
  type: string;
  required?: true;
  collate?: Collation;
  index?: IndexDescription;
  /** Indexes on paths into a json field. */
  paths?: { path: string; index: IndexDescription }[];
}
export type IndexDescription =
  | { kind: 'hash' | 'unique' | 'sorted' | 'inverted' }
  | { kind: 'ttl'; ms: number }
  | { kind: 'text'; k1?: number; b?: number; prefix?: number; prefix_min?: number; chars?: boolean }
  | {
      kind: 'hnsw';
      metric?: 'cosine' | 'l2' | 'dot';
      m?: number;
      ef_construction?: number;
      ef_search?: number;
      quant?: 'none' | 'int8' | 'bit';
    };

/** A difference the check did not apply, and how to resolve it. */
export interface SchemaRefusal {
  kind:
    | 'field_not_declared'
    | 'type_changed'
    | 'required_changed'
    | 'required_added'
    | 'collate_changed'
    | 'index_changed'
    | 'index_removed'
    | 'collection_missing'
    | 'field_missing'
    | 'index_missing';
  collection: string;
  /** The field, or the path into a json field; `null` for the collection. */
  field: string | null;
  message: string;
  fix: string;
}

/** What `checkSchema` found and did. */
export interface SchemaOutcome {
  kind: 'schema';
  /** Whether anything was written. */
  applied: boolean;
  /** Whether the migrations recorded ran: a database made from the code records them without running. */
  ran: boolean;
  /** The migrations recorded (planned: to be), by number from 1. */
  migrations: number[];
  /** The FenecQL that adds what is missing. */
  statements: string[];
  refusals: SchemaRefusal[];
}

/**
 * A schema written as FenecQL -- `create collection` and `create index`
 * statements, a `schema.fenecql` file -- as every SDK can hand it over. It
 * types nothing: the type is given (`Fenec.open<FenecSchema>`), or made by
 * `fenec types schema.fenecql`.
 */
export interface SchemaText {
  schema: string;
  /** Run once each, in order, before the rest is compared. */
  migrations?: readonly Migration[];
}

/** A database's schema declared in code, as `Fenec.open`, `openFile`, `sync` and `connect` take it. */
export interface SchemaOptions<T extends Tables = Tables, R extends Relations = Relations> {
  schema: T;
  /** `defineRelations`: what `lookup` names a relation by. */
  relations?: R;
  /** Run once each, in order, before the rest is compared. */
  migrations?: readonly Migration[];
}

// The schema constraint is `Record<keyof S, Fields>` rather than
// `Record<string, Fields>`: a hand-written `interface` has no implicit index
// signature and the latter would reject it. `fenec types` emits a `type`, but
// both have to work.
type AnySchema<S> = Record<keyof S, Fields>;

/** A row as read: the fields plus the automatic `id`. */
/** An aggregate of a select list, as FenecQL spells it. */
export type Aggregate<F extends Fields> =
  | 'count(*)'
  | `${'sum' | 'avg' | 'min' | 'max'}(${keyof F & string})`;

export type Row<F extends Fields> = F & { id: number };

/** Where a mark stands: `[start, end)` in UTF-16 code units, a string's index. */
export type Mark = [start: number, end: number];

/** A `snippet()` without tags: the window's text and its marks in it. */
export interface Snippet {
  text: string;
  marks: Mark[];
}

/** One value of a `facet` and how many matched rows hold it. */
export interface FacetCount<V = unknown> {
  value: V | null;
  count: number;
}

/** What `facet` counted: each field asked, its values most first. */
export type Facets = Record<string, FacetCount[]>;

/** The value a facet of a field counts: a list's elements one by one. */
export type FacetValue<T> = NonNullable<T> extends readonly (infer E)[] ? E : NonNullable<T>;

/** A query's rows, and when it asked for facets, the counts as `facets`. */
export type Rows<P, Fa> = {} extends Fa ? P[] : P[] & { facets: Fa };

/**
 * The row shape a chained `lookup` produces: the new collection's rows are
 * attached to the level named by `Path`, not to the parent.
 *
 * `Path` is the chain walked so far. Empty, the key lands on the parent row,
 * which is the single-level case; otherwise the first name is followed into
 * its element type and the rest of the path from there. Linear, so the
 * recursion is as deep as the chain and no deeper.
 */
type Attach<T, Path extends readonly string[], N extends string, C extends Fields> =
  Path extends readonly [infer H extends keyof T & string, ...infer R extends readonly string[]]
    ? Omit<T, H> & {
        [K in H]: T[H] extends readonly (infer E)[] ? Attach<E, R, N, C>[] : never;
      }
    : T & { [K in N]: Row<C>[] };

/**
 * A collation `order` can put text in: `'und'` is Unicode's order, for every
 * language (`collate und`); `'tr'` is Turkish (`collate tr`).
 */
export type Collation = 'und' | 'tr';

/**
 * The child side of a `lookup`. `on` is the child's field; the parent's key
 * is `id` unless `parentKey` names another. `order` takes `[field, dir]`
 * pairs -- `{ collate }` third, as `order()` takes it -- or a bare field name
 * for one ascending key.
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
  order?:
    | ChildKey<C>
    | Array<ChildKey<C> | [ChildKey<C>, ('asc' | 'desc')?, { collate?: Collation }?]>;
  limit?: number;
  offset?: number;
}

type ChildKey<C extends Fields> = keyof Row<C> & string;

/** The widened type accepted in write position. */
export type Writable<T> = T extends Timestamp
  ? Timestamp | string | number | Date
  : T extends Vector
    ? number[] | Float32Array
    : T extends Sparse
      ? string
    : T extends Bytes
      ? number[] | Uint8Array | string
      : T;

type Elem<T> = T extends readonly (infer U)[] ? U : never;

/**
 * Fields of type `vector<N>` or `sparse<N>` -- the only ones `near` accepts.
 * An optional field is generated as `Vector | null`, so `null` is peeled off
 * first.
 */
export type VectorKey<F extends Fields> = {
  [K in keyof F]: NonNullable<F[K]> extends Vector | Sparse ? K : never;
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
  /**
   * A list, or a query whose one column is the list: `from('customers')
   * .select('id').where(...)` runs once, before the outer query
   * (`in (get ...)`).
   */
  in?: Writable<T>[] | Query<any, any, any, any, any, any>;
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

declare const computed: unique symbol;

/**
 * A value a write works out over the row it writes: what `inc()` and
 * `expr()` make. Opaque, as a `Cond` is.
 */
export interface Computed {
  readonly [computed]: true;
}

/**
 * `{ n: inc(1) }` in an update: the field plus `by`, counting from 0 where
 * it is null (`n: coalesce(n, 0) + $1`), worked out under the write lock --
 * increments from many clients all land.
 */
export function inc(by?: number): Computed;

/**
 * A value as a FenecQL expression over the row, `?` placeholders bound to
 * parameters in order: `{ at: expr('now()') }`, `{ total: expr('price * ?', 1.2) }`.
 */
export function expr(sql: string, ...params: unknown[]): Computed;

/** Executor: `(sql, params)` -> response. A `Fenec` instance also works. */
export type Exec = (sql: string, params: unknown[]) => unknown;

export declare class FenecError extends Error {
  /**
   * Set when what refused the statement was collation data the module has
   * not been handed: the names to hand it (`Fenec.collation`).
   */
  collation?: string[];
  /** How many statements of the same text ran before this one. */
  ran?: number;
  /** A write the server refused (`FenecSync`): the HTTP status it answered. */
  status?: number;
  /** A write the server refused: the text of its first statement. */
  query?: string;
}

/**
 * Where the collation data the browser module does not carry comes from:
 * the URL of the directory holding `<name>.bin`, or a function handed the
 * name and returning its bytes or a `Response`.
 */
export type CollationSource =
  | string
  | URL
  | ((name: string) => BufferSource | Response | Promise<BufferSource | Response>);

/** The relations `defineRelations` gave the collection `At`, by name. */
type RelationsAt<R extends Relations, At extends string> = At extends keyof R ? R[At] : {};
type RelTarget<R extends Relations, At extends string, K> = K extends keyof RelationsAt<R, At>
  ? RelationsAt<R, At>[K] extends RelationRef<infer T>
    ? T
    : never
  : never;

/**
 * Immutable query builder. `F` is the collection's fields, `P` the result
 * of the projection so far, and `L` the chain of `lookup` names so far,
 * `Rel` the relations a schema in code declared and `At` the collection
 * the next `lookup` hangs off -- bookkeeping, never written out by a caller.
 */
export declare class Query<
  F extends Fields = Fields,
  P = Row<F>,
  L extends readonly string[] = [],
  Rel extends Relations = {},
  At extends string = string,
  Fa extends Facets = {},
> {
  /** Binds the query to an executor (wasm, HTTP, fenec-server). */
  bind(exec: Exec | { run(sql: string, params: unknown[]): unknown }): Query<F, P, L, Rel, At, Fa>;

  select<K extends keyof Row<F> & string>(
    ...cols: (K | K[])[]
  ): Query<F, Pick<Row<F>, K>, L, Rel, At, Fa>;
  /**
   * An aggregating list: the field grouped by, and aggregates spelled as
   * FenecQL spells them -- each answers under that name.
   *
   *   db.from('orders').select('status', 'count(*)', 'sum(total)').group('status')
   */
  select<K extends keyof Row<F> & string, A extends Aggregate<F>>(
    ...cols: (K | A)[]
  ): Query<F, Pick<Row<F>, K> & { [N in A]: number | string | null }, L, Rel, At, Fa>;
  select(): Query<F, Row<F>, L, Rel, At, Fa>;
  /** Fields and paths into json fields, each path answering under its text. */
  select<C extends (keyof Row<F> & string) | JsonPath<F>>(
    ...cols: C[]
  ): Query<F, { [N in C]: N extends keyof Row<F> ? Row<F>[N] : Json }, L, Rel, At, Fa>;

  /**
   * `highlight(field)` in the select list: where the terms `match` found
   * stand in the field's text, `[start, end]` pairs of UTF-16 offsets --
   * a JavaScript string's own -- or, given `{ pre, post }`, the text with
   * each mark between them, not escaped. Needs `match`; answers under
   * `highlight(<field>)`, after the fields `select` named.
   */
  highlight<K extends TextKey<F>>(
    field: K,
  ): Query<F, P & { [N in `highlight(${K})`]: Mark[] | null }, L, Rel, At, Fa>;
  highlight<K extends TextKey<F>>(
    field: K,
    opts: { pre: string; post: string },
  ): Query<F, P & { [N in `highlight(${K})`]: string | null }, L, Rel, At, Fa>;

  /**
   * `snippet(field, words)`: the window of `words` words around the densest
   * marks, `{ text, marks }` -- or, given `{ pre, post }`, the marked text --
   * with `ellipsis` where text is left out. Answers under
   * `snippet(<field>)`.
   */
  snippet<K extends TextKey<F>>(
    field: K,
    words: number,
    opts?: { ellipsis?: string },
  ): Query<F, P & { [N in `snippet(${K})`]: Snippet | null }, L, Rel, At, Fa>;
  snippet<K extends TextKey<F>>(
    field: K,
    words: number,
    opts: { ellipsis?: string; pre: string; post: string },
  ): Query<F, P & { [N in `snippet(${K})`]: string | null }, L, Rel, At, Fa>;

  /**
   * `facet field [top N]`: each value the field holds over every row the
   * query matches -- not only the page -- and how many rows hold it, most
   * first; a list counts once a row for each value. Beside the rows:
   * `rows().facets`, `run().facets`.
   */
  facet<K extends keyof Row<F> & string>(
    field: K,
    opts?: { top?: number },
  ): Query<F, P, L, Rel, At, Fa & { [N in K]: FacetCount<FacetValue<Row<F>[K]>>[] }>;
  facet<K extends JsonPath<F>>(
    field: K,
    opts?: { top?: number },
  ): Query<F, P, L, Rel, At, Fa & { [N in K]: FacetCount<Json>[] }>;

  /** `group field`: one row per value, for a select list that aggregates. */
  group(field: keyof Row<F> & string): Query<F, P, L, Rel, At, Fa>;

  where(cond: Where<F> | Cond<F>): Query<F, P, L, Rel, At, Fa>;
  where<K extends keyof Row<F> & string>(
    field: K,
    value: Writable<Row<F>[K]> | null | Spec<Row<F>[K]>,
  ): Query<F, P, L, Rel, At, Fa>;
  where<K extends keyof Row<F> & string>(
    field: K,
    op: 'in',
    values: Writable<Row<F>[K]>[],
  ): Query<F, P, L, Rel, At, Fa>;
  where<K extends keyof Row<F> & string>(
    field: K,
    op: 'has',
    value: Elem<Row<F>[K]>,
  ): Query<F, P, L, Rel, At, Fa>;
  where<K extends keyof Row<F> & string>(
    field: K,
    op: Op,
    value: Writable<Row<F>[K]> | null,
  ): Query<F, P, L, Rel, At, Fa>;
  /** A path into a json field: `where('meta.lang', '=', 'tr')`. */
  where(field: JsonPath<F>, value: Json | Spec<Json>): Query<F, P, L, Rel, At, Fa>;
  where(field: JsonPath<F>, op: 'in', values: Json[]): Query<F, P, L, Rel, At, Fa>;
  where(field: JsonPath<F>, op: Op, value: Json): Query<F, P, L, Rel, At, Fa>;

  orWhere(cond: Where<F> | Cond<F>): Query<F, P, L, Rel, At, Fa>;
  orWhere<K extends keyof Row<F> & string>(
    field: K,
    value: Writable<Row<F>[K]> | null | Spec<Row<F>[K]>,
  ): Query<F, P, L, Rel, At, Fa>;
  orWhere<K extends keyof Row<F> & string>(
    field: K,
    op: Op,
    value: unknown,
  ): Query<F, P, L, Rel, At, Fa>;

  /**
   * Vector search; `_score` is added to the result. Over a `sparse<N>` field
   * the vector is its text form and the score its dot product.
   */
  near(
    field: VectorKey<F>,
    vector: number[] | Float32Array | string,
    opts?: { ef?: number; exact?: boolean },
  ): Query<F, P & { _score: number }, L, Rel, At, Fa>;

  /** Full-text search over a `@text` index; `_score` is added to the result. */
  match(field: TextKey<F>, query: string): Query<F, P & { _score: number }, L, Rel, At, Fa>;

  /**
   * With both `match` and `near`: ranks by both. Each side takes its own
   * `candidates` (20 unless given, never fewer than the page) and a
   * document scores `1 / (k + rank)` from each list it is on (`k` 60).
   */
  fuse(opts?: { k?: number; candidates?: number }): Query<F, P, L, Rel, At, Fa>;

  /**
   * Reorders what `match` found by exact vector distance. Requires `match`,
   * but not an `@hnsw` index: the vectors are read out of the store.
   */
  rerank(
    field: VectorKey<F>,
    vector: number[] | Float32Array,
    opts?: { candidates?: number },
  ): Query<F, P & { _score: number }, L, Rel, At, Fa>;

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
   *
   * Calling it again chains rather than replaces: the second call binds to
   * the collection the first one named, and its rows are attached to *those*
   * rows. The type follows, which is why `Query` carries the chain so far --
   * `L` is bookkeeping, never written out by a caller.
   *
   *     db.from('shops')
   *       .lookup('orders', { on: 'shop_id' })
   *       .lookup('lines',  { on: 'order_id' })
   *     // -> { ...shop, orders: { ...order, lines: {...}[] }[] }[]
   *
   * `C` is never inferred from `opts`: taken from a `where` it was the
   * fields that `where` named and no others, and an `on` or an `order` by
   * any other field was refused.
   */
  lookup<M extends string, C extends Fields = Fields>(
    name: M,
    opts: LookupOptions<Hold<C>>,
  ): Query<F, Attach<P, L, M, C>, [...L, M], Rel, M, Fa>;
  /**
   * A relation of `defineRelations`, by its name: its key comes from the
   * relation, and its rows are typed by the table it reaches -- attached
   * under that table's name, as `lookup` attaches them.
   *
   *     db.from(products).lookup('reviews', { where: { stars: 5 } })
   */
  lookup<K extends keyof RelationsAt<Rel, At> & string>(
    name: K,
    opts?: Omit<LookupOptions<Hold<RelTarget<Rel, At, K>['$fields']>>, 'on' | 'parentKey'>,
  ): Query<
    F,
    Attach<P, L, RelTarget<Rel, At, K>['$name'], RelTarget<Rel, At, K>['$fields']>,
    [...L, RelTarget<Rel, At, K>['$name']],
    Rel,
    RelTarget<Rel, At, K>['$name'],
    Fa
  >;
  /** A table (`fenecTable`): its fields type the child rows. */
  lookup<T extends TableRef>(
    table: T,
    opts: LookupOptions<Hold<T['$fields']>>,
  ): Query<F, Attach<P, L, T['$name'], T['$fields']>, [...L, T['$name']], Rel, T['$name'], Fa>;

  /**
   * Successive calls add a sort key (the second decides when the first ties).
   * `{ collate: 'und' }` orders text as Unicode does for every language,
   * `{ collate: 'tr' }` as Turkish does, rather than by its bytes.
   */
  order(
    field: (keyof Row<F> & string) | Aggregate<F> | JsonPath<F>,
    dir?: 'asc' | 'desc',
    opts?: { collate?: Collation },
  ): Query<F, P, L, Rel, At, Fa>;
  limit(n: number): Query<F, P, L, Rel, At, Fa>;
  offset(n: number): Query<F, P, L, Rel, At, Fa>;

  /** The generated FenecQL and its parameters -- inspectable before running. */
  toFenecQL(): [sql: string, params: unknown[]];

  /** The text of the write statements, without running them. The write side of `toFenecQL`. */
  toInsert(docs: InsertRow<F> | InsertRow<F>[], opts?: { ifAbsent?: boolean }): [sql: string, params: unknown[]];
  toUpdate(patch: Insert<F>, opts?: { all?: boolean }): [sql: string, params: unknown[]];
  toDelete(opts?: { all?: boolean }): [sql: string, params: unknown[]];

  /** The query's collection. */
  readonly collection: string;
  /**
   * Every collection the query reads -- its own, each `lookup`'s, each inner
   * query's -- or `null` when a `raw` fragment may read more. A live query
   * runs again when one of them is written.
   */
  readonly reads: string[] | null;
  /** The opaque context carried by `bind` (for subclasses). */
  readonly context: unknown;
  /** The same body as a plain `Query`: bypasses subclass behaviour. */
  plain(): Query<F, P, L, Rel, At, Fa>;

  /** The raw response; `facets` holds what `facet` counted. */
  run(): Promise<{ columns: string[]; rows: P[]; facets?: Fa }>;
  /** The rows -- with what `facet` counted as `facets`, when it was asked. */
  rows(): Promise<Rows<P, Fa>>;
  first(): Promise<P | null>;
  /** Number of matching rows (`get ... count`); rows are not decoded. */
  count(): Promise<number>;
  /**
   * The path the query took (`explain get ...`): which index answered and
   * how many rows each stage read, one line a step. The query runs.
   */
  explain(): Promise<string[]>;

  /**
   * Writes the documents as FenecQL's `put`: new ones, and one naming an
   * `id` written over. FenecQL's `insert` refuses a taken id instead.
   * `{ ifAbsent: true }` passes over a document whose id or `@unique` value
   * is held (`put ... if absent`), and the count says what was written: a
   * lock taken (1) or not (0).
   */
  insert(docs: InsertRow<F> | InsertRow<F>[], opts?: { ifAbsent?: boolean }): Promise<number>;
  update(patch: Insert<F>, opts?: { all?: boolean }): Promise<number>;
  delete(opts?: { all?: boolean }): Promise<number>;
}

/** A patch (`update`): every field optional, a value written or worked out (`inc`, `expr`). */
export type Insert<F extends Fields> = {
  [K in keyof Row<F>]?: Writable<Row<F>[K]> | null | Computed;
};

/** The fields a write must give: those that read back never null (`required`, `.notNull()`). */
type RequiredKeys<F> = { [K in keyof F]-?: null extends F[K] ? never : K }[keyof F];

/**
 * A document as `insert` writes it: a required field given, the others
 * optional or null, and `id` only to write over the document holding it.
 */
export type InsertRow<F extends Fields> = { id?: number } & {
  [K in RequiredKeys<F>]: Writable<F[K]> | Computed;
} & {
  [K in Exclude<keyof F, RequiredKeys<F>>]?: Writable<F[K]> | null | Computed;
};

/**
 * Unbound builder: it only generates text, and `bind()` runs it. The
 * collection name is free here; the schema type is given per query as the
 * field type, or for all of them with `TypedFrom`.
 *
 *   from<FenecSchema['articles']>('articles').where('year', 2024)
 */
export function from<F extends Fields = Fields>(name: string): Query<F>;
export function from<T extends TableRef>(table: T): Query<T['$fields'], Row<T['$fields']>, [], {}, T['$name']>;

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
 * Remote fenecdb HTTP endpoint (`fenec-server --http`). The builder generates the
 * same FenecQL text; only the transport differs.
 */
export declare class FenecHttp<S extends AnySchema<S> = Schema, Rel extends Relations = {}> {
  constructor(url: string, opts?: HttpOptions);
  from<T extends TableRef>(table: T): Query<T['$fields'], Row<T['$fields']>, [], Rel, T['$name']>;
  from<K extends keyof S & string>(name: K): Query<S[K], Row<S[K]>, [], Rel, K>;
  run(sql: string, params?: unknown[]): Promise<any>;
  rows(sql: string, params?: unknown[]): Promise<any[]>;
  schemas(): Promise<{ collections?: SchemaInfo[] } | SchemaInfo[]>;
  /**
   * The server's database against a schema declared in code, in the
   * server (`/_schema`): `'follow'` says what the code declares and the
   * server lacks, `'plan'` what an apply would do, `'apply'` runs it -- the
   * last two with the server's token.
   */
  checkSchema(description: SchemaDescription | string, mode?: 'follow' | 'plan' | 'apply'): Promise<SchemaOutcome>;

  /** Where a live query's error goes when it has no `onError` of its own. */
  onError: ((e: unknown) => void) | null;

  /**
   * Live query over the server: `cb` is handed the rows once a
   * subscription to every collection the query reads is open, and again
   * after every write to one of them, the query run on the server each
   * time. A text names what it reads with `{collections}`, or is refused.
   * Returns the function that stops it.
   */
  live<F extends Fields, P, L extends readonly string[], R2 extends Relations, A extends string, Fa extends Facets>(
    query: Query<F, P, L, R2, A, Fa>,
    cb: (rows: Rows<P, Fa>) => void,
    opts?: LiveOptions,
  ): () => void;
  live(
    query: string | [sql: string, params: unknown[]],
    cb: (rows: any[]) => void,
    opts: LiveOptions & { collections: string[] },
  ): () => void;
}

/**
 * With a schema declared in code: the server's schema is checked against it,
 * and with `migrate` -- and the server's token -- brought to it. The server
 * owns its schema, so without `migrate` nothing is applied.
 */
export function connect<T extends Tables, R extends Relations = {}>(
  url: string,
  opts: HttpOptions & SchemaOptions<T, R> & { migrate?: boolean },
): Promise<FenecHttp<SchemaOf<T>, R>>;
export function connect<S extends AnySchema<S> = Schema>(
  url: string,
  opts: HttpOptions & SchemaText & { migrate?: boolean },
): Promise<FenecHttp<S>>;
export function connect<S extends AnySchema<S> = Schema>(
  url: string,
  opts?: HttpOptions,
): FenecHttp<S>;

export declare class Fenec<S extends AnySchema<S> = Schema, Rel extends Relations = {}> {
  /** Connects to a remote HTTP endpoint. */
  static connect<T extends Tables, R extends Relations = {}>(
    url: string,
    opts: HttpOptions & SchemaOptions<T, R> & { migrate?: boolean },
  ): Promise<FenecHttp<SchemaOf<T>, R>>;
  static connect<S extends AnySchema<S> = Schema>(
    url: string,
    opts: HttpOptions & SchemaText & { migrate?: boolean },
  ): Promise<FenecHttp<S>>;
  static connect<S extends AnySchema<S> = Schema>(
    url: string,
    opts?: HttpOptions,
  ): FenecHttp<S>;

  /**
   * Loads the WASM module. On Node the file bytes are passed directly:
   * `Fenec.open(await readFile('fenec.wasm'))`; in a Cloudflare Worker the
   * module as the Worker imports it, compiled:
   * `import wasm from './fenec.wasm'; Fenec.open(wasm)`. `collation` says
   * where the collation data it does not carry comes from: by default
   * `collate/` beside the module, when `src` is a URL.
   */
  static open<T extends Tables, R extends Relations = {}>(
    src: string | BufferSource | WebAssembly.Module | undefined,
    opts: { collation?: CollationSource } & SchemaOptions<T, R>,
  ): Promise<Fenec<SchemaOf<T>, R>>;
  /**
   * With `schema` -- tables declared in code -- the database is checked
   * against them: what only adds (a collection, a field, an index where
   * there is none) is made, the migrations not yet recorded run first, all
   * one block; anything that would lose data or could mean two things is
   * refused, and the open throws a `FenecError` naming every difference and
   * how to resolve it (`refusals`). `restore` and `openFile` check the
   * database they load again.
   */
  static open<S extends AnySchema<S> = Schema>(
    src?: string | BufferSource | WebAssembly.Module,
    opts?: { collation?: CollationSource } & Partial<SchemaText>,
  ): Promise<Fenec<S>>;

  readonly version: string;

  /**
   * The time, in milliseconds since the epoch, a statement is answered at:
   * a read of a collection whose rows expire (`@ttl`) leaves out those past
   * their time by it. `Date.now` unless set; the module has no clock.
   */
  now: () => number;

  /** Where a live query's error goes when it has no `onError` of its own. */
  onError: ((e: unknown) => void) | null;

  /** Query builder, over a collection by name or a table declared in code. */
  from<T extends TableRef>(table: T): Query<T['$fields'], Row<T['$fields']>, [], Rel, T['$name']>;
  from<K extends keyof S & string>(name: K): Query<S[K], Row<S[K]>, [], Rel, K>;

  /**
   * The database against a schema declared in code -- its description,
   * `describe(...)` -- in the engine: `'plan'` says what an apply would do
   * and writes nothing, `'apply'` runs the migrations not yet recorded and
   * makes what only adds, one block, or nothing while anything is refused.
   * `Fenec.open`'s `schema` is this, throwing on a refusal.
   */
  checkSchema(description: SchemaDescription | string, mode?: 'plan' | 'apply'): SchemaOutcome;

  /**
   * Live query: `cb` is handed the rows now, and again after every write
   * to a collection the query reads (a `load`, `restore` or `openFile`:
   * every live query). The writes of one task run it once, in a microtask
   * after it. Returns the function that stops it.
   */
  live<F extends Fields, P, L extends readonly string[], R2 extends Relations, A extends string, Fa extends Facets>(
    query: Query<F, P, L, R2, A, Fa>,
    cb: (rows: Rows<P, Fa>) => void,
    opts?: LiveOptions,
  ): () => void;
  live(query: string | [sql: string, params: unknown[]], cb: (rows: any[]) => void, opts?: LiveOptions): () => void;

  /**
   * Raw FenecQL -- synchronous. A statement that compares text in a
   * collation whose data for its script the module has not been handed is
   * refused (`FenecError.collation`); `query` fetches it and runs it again.
   */
  run(sql: string, params?: unknown[]): any;
  rows(sql: string, params?: unknown[]): any[];
  /** `run`, fetching the collation data a statement is refused for. */
  query(sql: string, params?: unknown[]): Promise<any>;
  /**
   * Hands the module the collation data `names` names -- `'all'` for every
   * script's -- before a statement needs it. The module carries `latin`.
   * True when it fetched any.
   */
  collation(...names: string[]): Promise<boolean>;

  schemas(): SchemaInfo[];
  stats(): unknown;
  snapshot(): Uint8Array;
  /**
   * The image a mebibyte at a time, each let go of in the module as it is
   * taken: never twice in the module, nor whole in the page.
   */
  snapshotChunks(): Generator<Uint8Array>;
  /**
   * Starts keeping the writes for `drain()`, or with `false` stops;
   * `persist` and `openFile` start it themselves.
   */
  journal(on?: boolean): void;
  /**
   * The writes since the last drain: frames to append to a stored image, or
   * with `replace` an image to store instead (after a `compact`).
   */
  drain(): { replace: boolean; bytes: Uint8Array };
  /**
   * Restores from a byte image, and returns how many of its bytes that took:
   * all, or those before a last record a crash cut short. Refused like `run`
   * for collation data.
   */
  load(bytes: Uint8Array): number;
  /** `load`, fetching the collation data the image needs. */
  loadAsync(bytes: Uint8Array): Promise<number>;
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

export interface LiveOptions {
  /** Where an error goes; else the database's `onError`, else thrown. */
  onError?: (e: unknown) => void;
  /** A text's parameters, given as a text alone. */
  params?: unknown[];
  /**
   * The collections a text reads: without them, every write runs it again.
   * Given for a builder query, they stand in for those it names.
   */
  collections?: string[];
}

export interface PersistOptions {
  /**
   * An AES-GCM key (`crypto.subtle.generateKey` or `deriveKey`): each
   * record is sealed with it, and a restore needs it.
   */
  cryptoKey?: CryptoKey | null;
}
/**
 * Writes the database into IndexedDB under `key`: the image the first time,
 * then only the writes since the last call. Returns the bytes written.
 */
export function persist(fenec: Fenec<any, any>, key?: string, opts?: PersistOptions): Promise<number>;
/** Restores from IndexedDB, image and chunks; `false` when there is no record. */
export function restore(fenec: Fenec<any, any>, key?: string, opts?: PersistOptions): Promise<boolean>;
/** Free-form state (cursors) -- next to the image, under a separate key. */
export function putState(key: string, value: unknown): Promise<void>;
export function getState(key: string): Promise<unknown>;

/** A database kept in a file of the origin private file system (`openFile`). */
export interface FenecFile {
  /** The file's name in its directory. */
  readonly name: string;
  /** Bytes in the file. */
  readonly size: number;
  /**
   * Writes what the database wrote since the last call into the file and
   * flushes it; `run` calls it after every statement. Returns the bytes
   * written. Once the file has refused a write, every later one is refused.
   */
  flush(): number;
  /** The file's bytes, which `fenec` and a server open. */
  bytes(): Uint8Array;
  /** Flushes and lets the file go: the database is kept in it no longer. */
  close(): void;
}

/**
 * Keeps the database in a file of the origin private file system, the bytes
 * fenec-server keeps on disk: a file holding some is loaded into the database
 * (which must hold nothing yet), an empty one takes its image, and from then
 * on `run` appends every statement's writes to it and flushes before it
 * answers. Only in a dedicated worker, one at a time a file.
 */
export function openFile(
  fenec: Fenec<any, any>,
  name?: string,
  opts?: { dir?: DirectoryHandle } & (Partial<SchemaOptions> | Partial<SchemaText>),
): Promise<FenecFile>;

/**
 * The DOM's `FileSystemDirectoryHandle`, where the program has the DOM's
 * types. A Worker's have none, and naming it outright failed every Worker
 * project that checks its libraries: there it is `never`, as there is no
 * file system to open a file of.
 */
type DirectoryHandle = typeof globalThis extends {
  FileSystemDirectoryHandle: { prototype: infer H };
}
  ? H
  : never;

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
  /** A collection's name, or its table declared in code. */
  collection: string | TableRef;
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
  /** Server root (`fenec-server --http`). */
  url: string;
  shapes: Shape<any>[];
  /** An existing local database; otherwise opened from `wasm`. */
  local?: Fenec<S>;
  /** The module the replica is opened with: `./fenec.wasm` unless given. */
  wasm?: string | BufferSource | WebAssembly.Module;
  /** Where the local module's collation data comes from (`Fenec.open`). */
  collation?: CollationSource;
  token?: string;
  /** Asked for a new token when the server answers 401; until then nothing is sent. */
  tokenProvider?: () => string | Promise<string>;
  fetch?: typeof globalThis.fetch;
  /**
   * IndexedDB key: the replica is stored there with its cursors and the
   * writes the server has not answered, which a page opened again sends.
   * A `local` kept in a file (`openFile`) keeps them in the file instead.
   */
  persist?: string;
  /** Seals the stored image and its chunks with AES-GCM (`persist`). */
  cryptoKey?: CryptoKey | null;
  /** `false` turns off multi-tab leader election. */
  leader?: 'auto' | false;
  /** Lock manager (defaults to `navigator.locks`). */
  locks?: { request(name: string, opts: unknown, fn: () => unknown): Promise<unknown> };
  onError?: (e: unknown) => void;
  /**
   * Told of each write the server refused -- any 4xx but 401, 408 and 429 --
   * once it was put back, a write left from a page before among them. A
   * network failure or a 5xx is no refusal: the write waits and goes again.
   */
  onRefused?: (e: FenecError & { status: number; query: string }) => void;
}

export interface ShapeStatus {
  collection: string;
  cursor: number;
  seeded: boolean;
  connected: boolean;
  /** Writes the server has not answered yet. */
  pending: number;
  error: string | null;
  leader: boolean;
}

/** Batch context: the same surface as `db`, but writes are accumulated. */
export interface Batch<S extends AnySchema<S> = Schema, Rel extends Relations = {}> {
  from<T extends TableRef>(table: T): Query<T['$fields'], Row<T['$fields']>, [], Rel, T['$name']>;
  from<K extends keyof S & string>(name: K): Query<S[K], Row<S[K]>, [], Rel, K>;
}

/**
 * Local replica + server connection. Reads are local (no network), writes
 * go local first and then to the server, feedback arrives over the subscription.
 */
export declare class FenecSync<S extends AnySchema<S> = Schema, Rel extends Relations = {}> {
  /** The local database; for raw FenecQL. */
  readonly local: Fenec<S, Rel>;
  /** The remote endpoint; for queries outside the shapes. */
  readonly remote: FenecHttp<S, Rel>;

  /** Resolves once the first seed of every shape has landed. */
  ready(): Promise<void>;
  /** Resolves once the server has answered every write made so far. */
  pushed(): Promise<void>;
  status(): ShapeStatus[];
  /** A new token: requests carry it from here on, and what a 401 stopped goes on. */
  setToken(token: string): void;
  /**
   * The network gone (`false`: the streams end, writes wait) or back
   * (`true`: what waited goes at once). A page has it from `online` and
   * `offline` on its own.
   */
  setOnline(online: boolean): void;

  /** Reads local, writes optimistic. A collection without a shape is rejected. */
  from<T extends TableRef>(table: T): Query<T['$fields'], Row<T['$fields']>, [], Rel, T['$name']>;
  from<K extends keyof S & string>(name: K): Query<S[K], Row<S[K]>, [], Rel, K>;

  /**
   * Live query: re-run after every local change, at the next frame.
   * `Fenec.live`'s contract. Returns: the function that ends the subscription.
   */
  live<F extends Fields, P, L extends readonly string[], R2 extends Relations, A extends string, Fa extends Facets>(
    query: Query<F, P, L, R2, A, Fa>,
    cb: (rows: Rows<P, Fa>) => void,
    opts?: LiveOptions,
  ): () => void;
  live(query: string | [sql: string, params: unknown[]], cb: (rows: any[]) => void, opts?: LiveOptions): () => void;

  /**
   * Several writes as **one**: applied together, sent as one `/batch` under
   * one key, landed by the server as one block or refused and put back
   * whole. Resolves with the number of statements once the server has them.
   */
  batch(fn: (t: Batch<S, Rel>) => Promise<void>): Promise<number>;

  /** Finishes any pending live-query runs (for tests). */
  flush(): Promise<void>;
  /** Closes the subscriptions and gives up the lead; the local database stays open. */
  close(): void;
}

/**
 * Opens the local replica and starts the subscriptions. With `schema`, the
 * code's schema is checked against the server's first and never applied:
 * the server owns it, and a replica takes its collections as the server
 * declares them.
 */
export function sync<T extends Tables, R extends Relations = {}>(
  opts: SyncOptions<any> & SchemaOptions<T, R>,
): Promise<FenecSync<SchemaOf<T>, R>>;
export function sync<S extends AnySchema<S> = Schema>(
  opts: SyncOptions<S> & Partial<SchemaText>,
): Promise<FenecSync<S>>;
