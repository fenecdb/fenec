// What the examples on javascript.html take from the page around them.

type DocsSchema = import('../fenec-schema.js').FenecSchema;

/** The database the page opened above the example. */
declare const db: import('../../../fenec.js').Fenec<DocsSchema>;
/** Vectors from the page's embedding model. */
declare const embedding: Float32Array;
declare const queryVector: Float32Array;
/** What a form asked for. */
declare const year: number | undefined;
declare const tag: string | undefined;
declare const userId: string;
declare const passphrase: string;
/** The search page's list and sidebar. */
declare const results: HTMLElement;
declare const sidebar: HTMLElement;
/** The page's own drawing, handed the rows. */
declare function draw(rows: { title: string | null }[]): void;
/** React's root (`createRoot`) and the component the example above defined. */
declare const root: { render(node: import('react').ReactNode): void };
declare function Todos(): import('react').ReactNode;
