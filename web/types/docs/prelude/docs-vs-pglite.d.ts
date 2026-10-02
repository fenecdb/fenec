// What the examples on vs-pglite.html take from the page around them.

/** A note's title and text, from a form. */
declare const title: string;
declare const body: string;
/** Vectors from the page's embedding model. */
declare const embedding: number[];
declare const queryVector: number[];
/** The page's own drawing, handed the rows. */
declare function draw(rows: { title: string | null }[]): void;
/** The table and the database the first example declared and opened. */
declare const notes: typeof import('../tables.js').notes;
declare const db: import('../../../fenec.js').Fenec<import('../../../fenec.js').SchemaOf<{ notes: typeof notes }>>;
