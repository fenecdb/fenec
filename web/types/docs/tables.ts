// The tables the docs' examples declare and then import from the app's own
// module ('./tables.js'): the same declarations, written once here.

import { fenecTable, text, boolean, timestamp, index } from '../../schema.js';

export const todos = fenecTable(
  'todos',
  { key: text(), title: text().notNull(), done: boolean(), at: timestamp() },
  (t) => [
    index('todos_key').using('hash', t.key),
    index('todos_done').using('hash', t.done),
    index('todos_at').on(t.at),
  ],
);
