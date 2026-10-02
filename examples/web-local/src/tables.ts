// The Notes schema in code, Drizzle's way: the same collection every
// example declares (examples/README.md). The open makes what is missing.
import { fenecTable, text, boolean, timestamp, vector, index } from '@fenecdb/web/schema';

export const notes = fenecTable('notes', {
  title: text(),
  body: text(),
  tags: text().array(),
  done: boolean(),
  at: timestamp(),
  embed: vector({ dimensions: 64 }),
}, (t) => [
  index('notes_body').using('bm25', t.body),
  index('notes_done').using('hash', t.done),
  index('notes_at').on(t.at),
  index('notes_embed').using('hnsw', t.embed.op('vector_cosine_ops')),
]);

export const SEEDS = [
  { title: 'Groceries', body: 'Buy milk, eggs and fresh bread for the weekend.', tags: ['home', 'shopping'], done: false, at: '2026-09-28T09:00:00Z' },
  { title: 'Release checklist', body: 'Tag the release, publish the packages and update the docs.', tags: ['work'], done: false, at: '2026-09-29T09:00:00Z' },
  { title: 'Book flights', body: 'Find cheap flights to Istanbul for the conference in spring.', tags: ['travel', 'work'], done: true, at: '2026-09-30T09:00:00Z' },
  { title: 'Book club', body: 'Finish the novel about the desert fox before Thursday.', tags: ['home', 'reading'], done: false, at: '2026-10-01T09:00:00Z' },
];

/**
 * A TOY embedding, a placeholder for a real model: hashed character
 * trigrams (FNV-1a over the UTF-8 bytes) into 64 dimensions. It matches
 * spelling, not meaning. A real one is an embeddings API call here, or
 * transformers.js (`pipeline('feature-extraction', ...)`), with the
 * vector's dimensions changed to match.
 */
export function embed(text: string): Float32Array {
  const bytes = new TextEncoder().encode(` ${text.replace(/[A-Z]/g, (c) => c.toLowerCase())} `);
  const v = new Float32Array(64);
  for (let i = 0; i + 3 <= bytes.length; i++) {
    let h = 0x811c9dc5;
    for (let j = i; j < i + 3; j++) h = Math.imul(h ^ bytes[j], 0x01000193) >>> 0;
    v[h % 64] += 1;
  }
  const norm = Math.hypot(...v);
  return norm ? v.map((x) => x / norm) : v;
}
