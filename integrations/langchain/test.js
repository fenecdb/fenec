// The store against a database in the page (a Fenec over web/fenec.wasm)
// and against fenec-server's HTTP endpoint (a FenecHttp): `npm test` here, after
// `npm ci`. What LangChain.js's own integrations are held to -- documents
// in and found again, their ids, scores, deletions, filters, a retriever --
// and BM25 and hybrid search. Needs web/fenec.wasm (make wasm); the HTTP
// half the fenec-server binary (cargo build -p fenec-server), and skips without it.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { access, mkdtemp, readFile, rm } from 'node:fs/promises';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Document } from '@langchain/core/documents';
import { SyntheticEmbeddings } from '@langchain/core/utils/testing';
import { Fenec, FenecHttp } from '../../web/fenec.js';
import { FenecVectorStore } from './index.js';

const root = new URL('../../', import.meta.url);
const wasm = await readFile(new URL('web/fenec.wasm', root)).catch(() => null);
const binary = new URL('target/debug/fenec-server', root).pathname;
const hasBinary = await access(binary).then(() => true, () => false);
const embeddings = new SyntheticEmbeddings({ vectorSize: 16 });

const port = () =>
  new Promise((res) => {
    const s = createServer().listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => res(port));
    });
  });

/** fenec-server over a file of its own, its HTTP endpoint up; `stop()` ends it. */
async function server() {
  const dir = await mkdtemp(join(tmpdir(), 'fenecdb-langchain-'));
  const [pg, http] = [await port(), await port()];
  const child = spawn(binary, ['--listen', `127.0.0.1:${pg}`, '--http', `127.0.0.1:${http}`, '--file', join(dir, 'db.fenec')], {
    stdio: 'ignore',
  });
  const url = `http://127.0.0.1:${http}`;
  for (let i = 0; ; i++) {
    try {
      await fetch(`${url}/_health`);
      break;
    } catch {
      if (i > 200) throw new Error('fenec-server did not start');
      await new Promise((r) => setTimeout(r, 50));
    }
  }
  return {
    client: new FenecHttp(url),
    async stop() {
      child.kill();
      await rm(dir, { recursive: true, force: true });
    },
  };
}

const docs = [
  new Document({ pageContent: 'fenecdb compacts a file beside the database', metadata: { source: 'handbook', page: 1 } }),
  new Document({ pageContent: 'a replica applies what the primary sends', metadata: { source: 'handbook', page: 2 } }),
  new Document({ pageContent: 'the browser keeps a database in IndexedDB', metadata: { source: 'blog', page: 3 } }),
  new Document({ pageContent: 'HNSW links each vector to its nearest', metadata: { source: 'paper', page: 4 } }),
];

/** Each test, once over a database in the page and once over HTTP. */
function both(name, fn) {
  test(`${name} (in the page)`, { skip: wasm ? false : 'no web/fenec.wasm (make wasm)' }, async () => {
    const db = await Fenec.open(wasm);
    try {
      await fn(db);
    } finally {
      db.close();
    }
  });
  test(`${name} (over HTTP)`, { skip: wasm && hasBinary ? false : 'no fenec-server binary (cargo build -p fenec-server)' }, async () => {
    const s = await server();
    try {
      await fn(s.client);
    } finally {
      await s.stop();
    }
  });
}

both('documents go in and are found again, their ids and scores kept', async (client) => {
  const store = new FenecVectorStore(embeddings, { client, collection: 'docs' });
  assert.deepEqual(await store.similaritySearch('anything', 2), [], 'a store nothing was written to is empty');
  const ids = await store.addDocuments(docs, { ids: ['a', 'b', 'c', 'd'] });
  assert.deepEqual(ids, ['a', 'b', 'c', 'd']);
  const [[best, score]] = await store.similaritySearchWithScore(docs[1].pageContent, 1);
  assert.equal(best.id, 'b');
  assert.equal(best.pageContent, docs[1].pageContent);
  assert.deepEqual(best.metadata, { source: 'handbook', page: 2 });
  assert.ok(Math.abs(score - 1) < 1e-5, `the same text scores 1 under cosine: ${score}`);
  assert.equal((await store.similaritySearch('replica', 3)).length, 3);

  // Written again under the same id: replaced, not added.
  await store.addDocuments([new Document({ pageContent: 'a replica follows its primary', metadata: { page: 9 } })], { ids: ['b'] });
  const again = await store.getByIds(['b', 'nope', 'a']);
  assert.deepEqual(again.map((d) => [d.id, d.metadata.page]), [['b', 9], ['a', 1]]);
  assert.equal((await store.similaritySearch('x', 10)).length, 4);

  // Ids made up for documents that came without.
  const made = await store.addDocuments([new Document({ pageContent: 'no id here' })]);
  assert.match(made[0], /^[0-9a-f-]{36}$/);

  await store.delete({ ids: ['a', made[0]] });
  assert.deepEqual((await store.similaritySearch('x', 10)).map((d) => d.id).sort(), ['b', 'c', 'd']);
  await store.delete({ deleteAll: true });
  assert.deepEqual(await store.similaritySearch('x', 10), []);
});

both('a filter over the metadata fields is answered by their index', async (client) => {
  const store = await FenecVectorStore.fromDocuments(docs, embeddings, {
    client,
    collection: 'filtered',
    metadataFields: { source: 'text', page: 'int' },
  });
  const sources = async (filter) =>
    (await store.similaritySearch('database', 10, filter)).map((d) => d.metadata.source).sort();
  assert.deepEqual(await sources({ source: 'handbook' }), ['handbook', 'handbook']);
  assert.deepEqual(await sources({ source: { $in: ['blog', 'paper'] } }), ['blog', 'paper']);
  assert.deepEqual(await sources({ source: ['blog'], page: { $eq: 3 } }), ['blog']);
  await assert.rejects(store.similaritySearch('x', 1, { author: 'me' }), /not in metadataFields/);
  await assert.rejects(store.similaritySearch('x', 1, { page: { $gt: 1 } }), /only \$eq and \$in/);
});

both('fromTexts, and a retriever over the store', async (client) => {
  const store = await FenecVectorStore.fromTexts(
    docs.map((d) => d.pageContent),
    docs.map((d) => d.metadata),
    embeddings,
    { client, collection: 'texts' },
  );
  const found = await store.asRetriever({ k: 2 }).invoke(docs[3].pageContent);
  assert.equal(found.length, 2);
  assert.equal(found[0].pageContent, docs[3].pageContent);
});

both('with fullText, by the words and by both', async (client) => {
  const store = await FenecVectorStore.fromDocuments(docs, embeddings, { client, collection: 'words', fullText: true });
  const [[first]] = await store.search('replica primary', 1, { mode: 'text' });
  assert.equal(first.pageContent, docs[1].pageContent);
  const hybrid = await store.search('browser IndexedDB', 2, { mode: 'hybrid' });
  assert.equal(hybrid[0][0].pageContent, docs[2].pageContent);
  const plain = new FenecVectorStore(embeddings, { client, collection: 'words' });
  await assert.rejects(plain.search('x', 1, { mode: 'text' }), /fullText/);
});

test('names and options that are not a store refuse to make one', () => {
  const client = { run() {} };
  assert.throws(() => new FenecVectorStore(embeddings, { collection: 'x' }), /client/);
  assert.throws(() => new FenecVectorStore(embeddings, { client, collection: 'no-dash' }), /collection name/);
  assert.throws(() => new FenecVectorStore(embeddings, { client, metric: 'hamming' }), /metric/);
  assert.throws(() => new FenecVectorStore(embeddings, { client, quant: 'bit', metric: 'l2' }), /cosine/);
  assert.throws(() => new FenecVectorStore(embeddings, { client, metadataFields: { content: 'text' } }), /metadata field/);
  assert.throws(() => new FenecVectorStore(embeddings, { client, metadataFields: { page: 'vector' } }), /a field is one of/);
});
