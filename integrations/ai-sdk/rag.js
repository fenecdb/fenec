// Retrieval for the Vercel AI SDK over fenecdb: its `embed` and `embedMany`
// make the vectors, fenecdb keeps them and finds the nearest. Nothing here
// but the SDK and a database: a `Fenec` keeps the index in the page (or a
// Worker), a `FenecHttp` the server's; both take the same builder.
//
//   const db = await Fenec.open('./fenec.wasm');
//   const model = openai.textEmbeddingModel('text-embedding-3-small');
//   await index(db, { collection: 'docs', model, documents });
//   const { text } = await generateText({
//     model: openai('gpt-4.1'),
//     tools: { search: searchTool(db, { collection: 'docs', model }) },
//     stopWhen: stepCountIs(4),
//     prompt: 'How do I compact the database?',
//   });
//
// A copy of this file is the integration: the AI SDK has no vector store
// interface to implement, only the embedding models and `tool`.

import { embed, embedMany, jsonSchema, tool } from 'ai';

const NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

/**
 * Embeds `documents` -- `{text, source?}` each -- and writes them into
 * `collection`, made on the first call with the embedding's size: the text
 * indexed for BM25 as well, so `retrieve` can fuse the words with the
 * vector. Returns how many were written.
 */
export async function index(db, { collection, model, documents }) {
  if (!NAME.test(collection)) throw new Error(`not a collection name: ${collection}`);
  if (!documents.length) return 0;
  const { embeddings } = await embedMany({ model, values: documents.map((d) => d.text) });
  await db.run(
    `create collection if not exists ${collection} ` +
      `(text text @text, source text @hash, embedding vector<${embeddings[0].length}> @hnsw(cosine))`,
    [],
  );
  const rows = documents.map((d, i) => ({ text: d.text, source: d.source ?? null, embedding: embeddings[i] }));
  return db.from(collection).insert(rows);
}

/**
 * The `k` documents nearest `query`: by the vector alone, or with
 * `hybrid` by the vector and the words, each ranked to its own depth and
 * the two rankings fused. `source` keeps to one, through its index.
 */
export async function retrieve(db, { collection, model, query, k = 4, hybrid = false, source }) {
  const { embedding } = await embed({ model, value: query });
  let q = db.from(collection).select('text', 'source');
  if (source !== undefined) q = q.where('source', source);
  q = hybrid ? q.match('text', query).near('embedding', embedding).fuse() : q.near('embedding', embedding);
  return q.limit(k).rows();
}

/** A tool a model calls to look things up: `retrieve` behind it. */
export function searchTool(db, { collection, model, k = 4, hybrid = true }) {
  return tool({
    description: `Searches the ${collection} documents for passages relevant to a question.`,
    inputSchema: jsonSchema({
      type: 'object',
      properties: { query: { type: 'string', description: 'what to look for' } },
      required: ['query'],
    }),
    execute: async ({ query }) =>
      (await retrieve(db, { collection, model, query, k, hybrid })).map((r) => ({ text: r.text, source: r.source })),
  });
}
