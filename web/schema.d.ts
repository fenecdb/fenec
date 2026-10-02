// Type declarations for `@fenecdb/web/schema`: a database's schema declared
// in TypeScript, Drizzle ORM 1.0's way, compiled to the description every
// fenecdb SDK compiles its declarations to. `web/fenec.d.ts` says how it is
// opened and checked.

import type {
  Bytes,
  CollectionDescription,
  Collation,
  FieldDescription,
  Fields,
  InsertRow,
  Json,
  Migration,
  RelationRef,
  Row,
  SchemaDescription,
  Sparse,
  TableRef,
  Tables,
  Timestamp,
  Vector,
} from './fenec.js';

type Simplify<T> = { [K in keyof T]: T[K] } & {};

/** The description's format. */
export declare const FORMAT: 1;

// ----------------------------------------------------------------- columns

type Kind = 'text' | 'int' | 'float' | 'bool' | 'timestamp' | 'bytes' | 'json' | 'vector' | 'sparse' | 'array';

/**
 * A column: what a field reads as (`T`), whether `.notNull()` made it
 * required (`NN`), and its kind, which says which modifiers it takes.
 * Nullable unless `.notNull()`, as in Drizzle -- FenecQL's `required`.
 */
export interface Column<T = unknown, NN extends boolean = boolean, K extends Kind = Kind> {
  /** Types alone: never read at run time. */
  readonly _: { readonly data: T; readonly notNull: NN; readonly kind: K };
  /** The field's name, the key it is declared under. */
  readonly name: string;
  /** `required`: every write gives it, and it never reads null. */
  notNull(): Col<T, true, K>;
  /** `@unique`: the field's one index, as `uniqueIndex().on(col)` makes it. */
  unique(): Col<T, NN, K>;
  /** What the column reads as, for TypeScript alone: a json column's shape. */
  $type<U>(): Col<U, NN, K>;
  // No `default`, `primaryKey` or `references`: FenecQL has no column
  // defaults, every document has an `id` of its own, and a relation is
  // `defineRelations`. Called from JavaScript, each throws saying so.
  toJSON(): FieldDescription;
}

/** A column with what its kind adds. */
export type Col<T, NN extends boolean, K extends Kind> = Column<T, NN, K> &
  (K extends 'text'
    ? {
        /** `collate tr` or `collate und`: the order its text compares in. */
        collate(collation: Collation): Col<T, NN, K>;
        array(): Col<T[], NN, 'array'>;
      }
    : K extends 'json'
      ? {
          /** A path into the json column, for an index: `t.meta.path('source.rank')`. */
          path(keys: string): PathRef;
        }
      : K extends 'vector'
        ? {
            /** The column under one of pgvector's operator classes, for `index().using('hnsw', ...)`. */
            op(opclass: VectorOp): VectorOpRef;
          }
        : K extends 'sparse' | 'array'
          ? {}
          : { array(): Col<T[], NN, 'array'> });

/** Any column of any table. */
export type AnyColumn = Column<any, boolean, Kind>;

/** pgvector's operator classes: cosine, l2 and inner product (`dot`). */
export type VectorOp =
  | 'vector_cosine_ops'
  | 'vector_l2_ops'
  | 'vector_ip_ops'
  | 'halfvec_cosine_ops'
  | 'halfvec_l2_ops'
  | 'halfvec_ip_ops';

export interface PathRef {
  readonly column: AnyColumn;
  readonly path: string;
}
export interface VectorOpRef {
  readonly column: AnyColumn;
  readonly metric: 'cosine' | 'l2' | 'dot';
}

/** `text`. */
export function text(): Col<string, false, 'text'>;
/** `int`: a 64-bit integer, read as a number. */
export function integer(): Col<number, false, 'int'>;
/** `float`: an f64, PostgreSQL's double precision. */
export function doublePrecision(): Col<number, false, 'float'>;
/** `bool`. */
export function boolean(): Col<boolean, false, 'bool'>;
/** `timestamp`: read as ISO-8601 text; written as a Date, text or milliseconds. */
export function timestamp(): Col<Timestamp, false, 'timestamp'>;
/** `bytes`. */
export function bytea(): Col<Bytes, false, 'bytes'>;
/** `bytes`, by fenecdb's own name. */
export function bytes(): Col<Bytes, false, 'bytes'>;
/** `json`: any value JSON holds. */
export function json(): Col<Json, false, 'json'>;
/** `json`: fenecdb has one json type, which is jsonb over the wire. */
export function jsonb(): Col<Json, false, 'json'>;
/** `vector<N>`, pgvector's `vector`. */
export function vector(opts: { dimensions: number }): Col<Vector, false, 'vector'>;
/** `vector<N, f16>`, pgvector's `halfvec`: half the memory a component. */
export function halfvec(opts: { dimensions: number }): Col<Vector, false, 'vector'>;
/** `sparse<N>`, pgvector's `sparsevec`. */
export function sparsevec(opts: { dimensions: number }): Col<Sparse, false, 'sparse'>;

// ----------------------------------------------------------------- indexes

/** `@hnsw`'s options, as `.with()` takes them. */
export interface HnswOptions {
  m?: number;
  ef_construction?: number;
  ef_search?: number;
  /** What the index holds of each vector: whole, a byte a component, or a bit. */
  quant?: 'none' | 'int8' | 'bit';
}
/** `@text`'s options, as `.with()` takes them. */
export interface Bm25Options {
  k1?: number;
  b?: number;
  /** Also indexes each word's prefixes up to this long: a stemmer for an inflected language. */
  prefix?: number;
  prefix_min?: number;
  /** Also each character of Han, kana and Hangul. */
  chars?: boolean;
}
/** A duration: `'45s'`, `'30m'`, `'12h'`, `'7d'`. */
export type Duration = `${number}${'ms' | 's' | 'm' | 'h' | 'd'}`;

/** An index, made: what the third argument of `fenecTable` returns. */
export interface IndexDef<O = never> {
  readonly method: string;
  /** Options, by the method: `HnswOptions` for hnsw, `Bm25Options` for bm25. */
  with(options: O): IndexDef<O>;
}
/** An ordered index, whose rows `.ttl()` makes expire. */
export interface OrderedIndexDef extends IndexDef {
  /** `@ttl`: a row is gone this long after its timestamp. */
  ttl(duration: Duration): IndexDef;
}

type TextColumn = Column<string | string[], boolean, 'text' | 'array'>;

export interface IndexBuilder {
  /** `@sorted`, a btree's work. */
  on(target: AnyColumn | PathRef): OrderedIndexDef;
  using(method: 'btree', target: AnyColumn | PathRef): OrderedIndexDef;
  /** `@hash`: equality, and a `lookup`'s key. */
  using(method: 'hash', target: AnyColumn | PathRef): IndexDef;
  /** `@hnsw`: nearest neighbours, by the operator class's distance. */
  using(method: 'hnsw', target: VectorOpRef | Column<Vector, boolean, 'vector'>): IndexDef<HnswOptions>;
  /** `@text`: ranked text search, BM25 (fenecdb's own). */
  using(method: 'bm25', target: TextColumn): IndexDef<Bm25Options>;
  /** `@inverted`: a sparse vector's dot product (fenecdb's own). */
  using(method: 'inverted', target: Column<Sparse, boolean, 'sparse'>): IndexDef;
}
export interface UniqueIndexBuilder {
  /** `@unique`: a hash refusing a value held twice. */
  on(target: AnyColumn | PathRef): IndexDef;
  using(method: 'hash' | 'btree', target: AnyColumn | PathRef): IndexDef;
}

/**
 * An index. The name is Drizzle's, and fenecdb has none: it is checked to
 * be the table's one of it, and kept nowhere.
 */
export function index(name?: string): IndexBuilder;
export function uniqueIndex(name?: string): UniqueIndexBuilder;

// ------------------------------------------------------------------ tables

/** What each column of `C` reads as: `fenec types`'s fields for the collection. */
export type FieldsOf<C> = {
  [K in keyof C]: C[K] extends Column<infer T, infer NN, any> ? (NN extends true ? T : T | null) : never;
};

/** A table: its columns by name, and its types. */
export type FenecTable<N extends string = string, C extends Record<string, AnyColumn> = Record<string, AnyColumn>> = C &
  TableRef<N, FieldsOf<C>> & {
    /** A row as read, `id` and all: what `Row<FenecSchema[N]>` is. */
    readonly $inferSelect: Simplify<Row<FieldsOf<C>>>;
    /** A row as written by `insert`: the `.notNull()` fields given. */
    readonly $inferInsert: Simplify<InsertRow<FieldsOf<C>>>;
    toJSON(): CollectionDescription;
    /** The collection as FenecQL: `create collection`, then a `create index` a path. */
    toFenecQL(): string;
  };

/**
 * A collection, as `pgTable` declares a table: its columns, and its indexes
 * in the third argument. `id` is every document's own and is not declared.
 */
export function fenecTable<N extends string, C extends Record<string, AnyColumn>>(
  name: N,
  columns: C,
  extra?: (t: C) => IndexDef<any>[],
): FenecTable<N, C>;
/** `fenecTable` by fenecdb's own word. */
export const collection: typeof fenecTable;

/** The table's columns by name, as Drizzle's `getColumns`. */
export function getColumns<C extends Record<string, AnyColumn>>(table: FenecTable<string, C>): C;

/** The description of `schema` and its migrations: what `checkSchema` and `/_schema` take. */
export function describe(schema: Tables, migrations?: readonly Migration[]): SchemaDescription;
/**
 * The schema as FenecQL, `schema.fenecql`: each table's `create collection`
 * and the `create index` of each path. What an open hands the engine.
 */
export function toFenecQL(schema: Tables): string;

// -------------------------------------------------------------- migrations

type TableOrName = TableRef | string;
/** `alter collection <t> rename field <from> to <to>`. */
export function rename(table: TableOrName, from: string, to: string): string;
/** `alter collection <t> drop field <field>`. */
export function drop(table: TableOrName, field: string): string;
/** `drop collection <t>`. */
export function dropTable(table: TableOrName): string;
/** A field made again as the code declares it, its values copied: what changes its index or collation. */
export function rebuild(table: TableOrName, field: string): { rebuild: { collection: string; field: string } };

// ---------------------------------------------------------------- relations

/** A column of a table, as `defineRelations`' `r.<table>.<column>` names it; `id` among them. */
export interface ColumnRef<T extends TableRef = TableRef> {
  readonly table: T;
  readonly name: string;
}

/** A relation: `many` attaches every child, `one` the first. */
export interface Relation<T extends TableRef = TableRef, K extends 'many' | 'one' = 'many' | 'one'>
  extends RelationRef<T> {
  readonly $kind: K;
}

type RelationHelpers<T extends Tables> = {
  [K in keyof T]: { readonly [C in keyof T[K]['$fields'] | 'id']: ColumnRef<T[K]> };
} & {
  many: { [K in keyof T]: (spec: { from: ColumnRef; to: ColumnRef<T[K]> }) => Relation<T[K], 'many'> };
  one: { [K in keyof T]: (spec: { from: ColumnRef; to: ColumnRef<T[K]> }) => Relation<T[K], 'one'> };
};

/**
 * Drizzle 1.0's `defineRelations`, answered by `lookup`: a relation is
 * `on <to> = <from>`, so `to` is the child's key -- `id`, or a column with a
 * hash or unique index -- and a relation `lookup` cannot answer is refused.
 * The rows it attaches are under the table's name, a list either way.
 *
 *   const relations = defineRelations({ products, reviews }, (r) => ({
 *     products: { reviews: r.many.reviews({ from: r.products.id, to: r.reviews.productId }) },
 *   }));
 *   const db = await Fenec.open(wasm, { schema: { products, reviews }, relations });
 *   await db.from(products).lookup('reviews').rows();
 */
export function defineRelations<T extends Tables, C extends { [K in keyof T]?: Record<string, Relation> }>(
  tables: T,
  relations: (r: RelationHelpers<T>) => C,
): { [K in keyof C & keyof T as T[K]['$name']]: C[K] & {} };
