// What the examples on sync.html take from the page around them.

/** The replica the page opened above the example. */
declare const db: import('../../../fenec.js').FenecSync<import('../fenec-schema.js').FenecSchema>;
/** The page's own drawing, handed the rows. */
declare function draw(rows: { title: string | null }[]): void;
/** The page's sign-in, which hands out a fresh token. */
declare const auth: { refresh(): Promise<string> };
/** The page's own way of telling its user something. */
declare function toast(text: string): void;
