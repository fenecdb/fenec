// The tables the docs' examples declare and then import from the app's own
// module ('./tables.js'): the same declarations, written once here.

import { fenecTable, text, boolean, timestamp, vector, index } from '../../schema.js';

export const todos = fenecTable(
  'todos',
  { key: text(), title: text().notNull(), done: boolean(), at: timestamp() },
  (t) => [
    index('todos_key').using('hash', t.key),
    index('todos_done').using('hash', t.done),
    index('todos_at').on(t.at),
  ],
);

// vs-pglite.html: a note with a text and a vector index.
export const notes = fenecTable(
  'notes',
  { title: text(), body: text(), embed: vector({ dimensions: 384 }) },
  (t) => [
    index('notes_body').using('bm25', t.body),
    index('notes_embed').using('hnsw', t.embed.op('vector_cosine_ops')),
  ],
);
