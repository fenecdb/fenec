// rag.js against the AI SDK's own mock models: `npm test` here, after
// `npm ci`. Documents embedded with `embedMany` and found with `near`,
// kept to a source, fused with the words; then a model calling the search
// tool in `generateText` and answering from what it found. Needs
// web/fenec.wasm (make wasm).

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { generateText, stepCountIs } from 'ai';
import { MockEmbeddingModelV4, MockLanguageModelV4 } from 'ai/test';
import { Fenec } from '../../web/fenec.js';
import { index, retrieve, searchTool } from './rag.js';

const wasm = await readFile(new URL('../../web/fenec.wasm', import.meta.url)).catch(() => null);
const skip = wasm ? false : 'no web/fenec.wasm (make wasm)';

// A text's vector: how often each letter a..z appears, so texts sharing
// words lie near each other, as a real model's would.
const letters = (text) => {
  const v = new Array(26).fill(0);
  for (const c of text.toLowerCase()) {
    const i = c.charCodeAt(0) - 97;
    if (i >= 0 && i < 26) v[i]++;
  }
  return v;
};
const model = new MockEmbeddingModelV4({
  doEmbed: async ({ values }) => ({ embeddings: values.map(letters) }),
});

const documents = [
  { text: 'compact rewrites the file beside the database', source: 'handbook' },
  { text: 'a replica applies what its primary sends', source: 'handbook' },
  { text: 'the browser keeps a database in indexeddb', source: 'blog' },
  { text: 'zebras graze quietly', source: 'zoo' },
];

test('documents embedded with the SDK are found again', { skip }, async () => {
  const db = await Fenec.open(wasm);
  assert.equal(await index(db, { collection: 'docs', model, documents }), 4);
  const near = await retrieve(db, { collection: 'docs', model, query: 'compact rewrites the file', k: 2 });
  assert.equal(near[0].text, documents[0].text);
  assert.ok(near[0]._score > near[1]._score);
  const blog = await retrieve(db, { collection: 'docs', model, query: 'compact', k: 4, source: 'blog' });
  assert.deepEqual(blog.map((r) => r.source), ['blog']);
  const hybrid = await retrieve(db, { collection: 'docs', model, query: 'replica primary', k: 2, hybrid: true });
  assert.equal(hybrid[0].text, documents[1].text);
  // A second batch goes into the same collection.
  assert.equal(await index(db, { collection: 'docs', model, documents: [{ text: 'zebras again' }] }), 1);
  assert.equal(await db.from('docs').count(), 5);
  await assert.rejects(index(db, { collection: 'no-dash', model, documents }), /collection name/);
});

test('a model calls the search tool and answers from what it found', { skip }, async () => {
  const db = await Fenec.open(wasm);
  await index(db, { collection: 'docs', model, documents });
  let step = 0;
  const llm = new MockLanguageModelV4({
    doGenerate: async ({ prompt }) => {
      step++;
      const done = { usage: { inputTokens: { total: 1 }, outputTokens: { total: 1 } }, warnings: [] };
      if (step === 1) {
        return {
          ...done,
          content: [{ type: 'tool-call', toolCallId: 'c1', toolName: 'search', input: JSON.stringify({ query: 'how does compact work' }) }],
          finishReason: { unified: 'tool-calls', raw: undefined },
        };
      }
      const found = prompt.at(-1).content[0].output.value;
      return { ...done, content: [{ type: 'text', text: `From the ${found[0].source}: ${found[0].text}` }], finishReason: { unified: 'stop', raw: undefined } };
    },
  });
  const { text, steps } = await generateText({
    model: llm,
    tools: { search: searchTool(db, { collection: 'docs', model, k: 2 }) },
    stopWhen: stepCountIs(3),
    prompt: 'How do I compact the database?',
  });
  assert.equal(steps.length, 2);
  assert.equal(text, `From the handbook: ${documents[0].text}`);
});
