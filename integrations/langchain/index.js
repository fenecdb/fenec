// A LangChain.js vector store over fenecdb: the database in the page (a
// `Fenec` from @fenecdb/web) or behind fenec-pg's HTTP endpoint (a
// `FenecHttp`) -- anything with `run(sql, params)`.
//
//   import { FenecVectorStore } from '@fenecdb/langchain';
//   const store = new FenecVectorStore(embeddings, { client: db, collection: 'docs' });
//   await store.addDocuments(documents);
//   await store.similaritySearch('how do I compact', 4);
//
// The same store as fenecdb's Python one (integrations/python), the same
// collection: a document is a row -- its id, its text, its metadata as
// JSON and its vector under `@hnsw`, over int8 or bit codes with `quant`.
// The collection is made on the first write, when the embedding's size is
// known. A metadata field named in `metadataFields` also gets a column of
// its own under `@hash`, and `filter` takes equality over those -- `{source:
// 'handbook'}` is `where source = $2`, answered by the index before the
// search ranks what passed. With `fullText` the text is indexed for BM25
// as well, and `search(query, k, {mode})` ranks by the words (`'text'`) or
// by the words and the vector fused (`'hybrid'`).

import { VectorStore } from '@langchain/core/vectorstores';
import { Document } from '@langchain/core/documents';

// Names go into the statement's text, so they are held to FenecQL's.
const NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;
const TYPES = new Set(['text', 'int', 'float', 'bool', 'timestamp']);
const RESERVED = new Set(['id', 'lc_id', 'content', 'metadata', 'embedding']);
const METRICS = new Set(['cosine', 'l2', 'dot']);
const QUANT = new Set([undefined, null, 'int8', 'bit']);

/** `$first, $first+1, ...`: `n` of them, for an `in [...]`. */
const placeholders = (first, n) => Array.from({ length: n }, (_, i) => `$${first + i}`).join(', ');

export class FenecVectorStore extends VectorStore {
  /**
   * @param {import('@langchain/core/embeddings').EmbeddingsInterface} embeddings
   * @param {{client: {run(sql: string, params?: unknown[]): unknown}, collection?: string,
   *   metric?: 'cosine'|'l2'|'dot', metadataFields?: Record<string, string>,
   *   quant?: 'int8'|'bit', fullText?: boolean}} args
   */
  constructor(embeddings, args) {
    super(embeddings, args);
    const { client, collection = 'langchain', metric = 'cosine', metadataFields = {}, quant, fullText = false } = args ?? {};
    if (!client || typeof client.run !== 'function') {
      throw new Error('client: a Fenec or FenecHttp from @fenecdb/web, or anything with run(sql, params)');
    }
    if (!NAME.test(collection)) throw new Error(`not a collection name: ${collection}`);
    if (!METRICS.has(metric)) throw new Error(`metric is one of cosine, l2, dot, not ${metric}`);
    if (!QUANT.has(quant)) throw new Error(`quant is 'int8' or 'bit', not ${quant}`);
    if (quant === 'bit' && metric !== 'cosine') {
      throw new Error("quant 'bit' keeps signs, which only cosine can order by");
    }
    for (const [name, ty] of Object.entries(metadataFields)) {
      if (!NAME.test(name) || RESERVED.has(name)) throw new Error(`not a metadata field this store can index: ${name}`);
      if (!TYPES.has(ty)) throw new Error(`${name}: a field is one of ${[...TYPES]}, not ${ty}`);
    }
    this.client = client;
    this.collection = collection;
    this.metric = metric;
    this.fields = metadataFields;
    this.quant = quant ?? null;
    this.fullText = fullText;
    this.dimension = null;
  }

  _vectorstoreType() {
    return 'fenecdb';
  }

  // ------------------------------------------------------------ writing

  async addDocuments(documents, options) {
    const vectors = await this.embeddings.embedDocuments(documents.map((d) => d.pageContent));
    return this.addVectors(vectors, documents, options);
  }

  /**
   * Writes the documents with their vectors. An id already there is
   * replaced: its row taken out, then the new one put. On a `Fenec` the two
   * run as one block; over HTTP they are two requests.
   */
  async addVectors(vectors, documents, options = {}) {
    if (!documents.length) return [];
    const ids = documents.map((d, i) => options.ids?.[i] ?? d.id ?? crypto.randomUUID());
    await this.#ensure(vectors[0].length);
    const names = ['lc_id', 'content', 'metadata', 'embedding', ...Object.keys(this.fields)];
    const docs = [];
    const params = [];
    documents.forEach((doc, i) => {
      const meta = doc.metadata ?? {};
      const values = [ids[i], doc.pageContent, JSON.stringify(meta), Array.from(vectors[i], Number)];
      for (const f of Object.keys(this.fields)) values.push(meta[f] ?? null);
      const first = params.length + 1;
      docs.push(`{${names.map((n, j) => `${n}: $${first + j}`).join(', ')}}`);
      params.push(...values);
    });
    const del = `del ${this.collection} where lc_id in [${placeholders(1, ids.length)}]`;
    const put = `put ${this.collection} [${docs.join(', ')}]`;
    await this.client.run(del, ids);
    await this.client.run(put, params);
    return ids;
  }

  /** Deletes these ids, or with `deleteAll` every document. */
  async delete({ ids, deleteAll } = {}) {
    if (deleteAll) return void (await this.#run(`del ${this.collection}`, []));
    if (!ids?.length) return;
    await this.#run(`del ${this.collection} where lc_id in [${placeholders(1, ids.length)}]`, ids);
  }

  /** Drops the collection, documents and index together. */
  async dropCollection() {
    await this.client.run(`drop collection if exists ${this.collection}`, []);
    this.dimension = null;
  }

  // ------------------------------------------------------------ reading

  async getByIds(ids) {
    if (!ids.length) return [];
    const rows = await this.#run(
      `get ${this.collection} select lc_id, content, metadata where lc_id in [${placeholders(1, ids.length)}]`,
      ids,
    );
    const byId = new Map(rows.map((r) => [r.lc_id, this.#document(r)]));
    return ids.filter((id) => byId.has(id)).map((id) => byId.get(id));
  }

  /**
   * `[document, score]` pairs nearest `query`: the cosine similarity under
   * `cosine`, higher is nearer; the distance under `l2`; the product under
   * `dot`.
   */
  async similaritySearchVectorWithScore(query, k, filter) {
    const [where, params] = this.#where(filter, 2);
    const rows = await this.#run(
      `get ${this.collection} select lc_id, content, metadata${where} near embedding $1 limit ${k | 0}`,
      [Array.from(query, Number), ...params],
    );
    return rows.map((r) => [this.#document(r), r._score]);
  }

  /**
   * With `fullText`: `mode` `'text'` ranks by the words alone, no embedding
   * of the query; `'hybrid'` ranks the words and the vector each to their
   * own depth and fuses the rankings. The scores are BM25's, or the
   * fusion's. `'similarity'` is `similaritySearchWithScore`.
   */
  async search(query, k = 4, { mode = 'similarity', filter } = {}) {
    if (mode === 'similarity') return this.similaritySearchWithScore(query, k, filter);
    if (!this.fullText) throw new Error(`mode ${mode} needs the store made with fullText`);
    let rank;
    let ahead;
    if (mode === 'text') {
      [rank, ahead] = ['match content $1', [query]];
    } else if (mode === 'hybrid') {
      const vector = await this.embeddings.embedQuery(query);
      [rank, ahead] = ['match content $1 near embedding $2 fuse', [query, Array.from(vector, Number)]];
    } else {
      throw new Error(`mode is similarity, text or hybrid, not ${mode}`);
    }
    const [where, params] = this.#where(filter, ahead.length + 1);
    const rows = await this.#run(
      `get ${this.collection} select lc_id, content, metadata${where} ${rank} limit ${k | 0}`,
      [...ahead, ...params],
    );
    return rows.map((r) => [this.#document(r), r._score]);
  }

  static async fromTexts(texts, metadatas, embeddings, args) {
    const docs = texts.map(
      (text, i) => new Document({ pageContent: text, metadata: Array.isArray(metadatas) ? (metadatas[i] ?? {}) : (metadatas ?? {}) }),
    );
    return FenecVectorStore.fromDocuments(docs, embeddings, args);
  }

  static async fromDocuments(docs, embeddings, args) {
    const store = new FenecVectorStore(embeddings, args);
    await store.addDocuments(docs);
    return store;
  }

  // ------------------------------------------------------------ inside

  async #ensure(dimension) {
    if (this.dimension === dimension) return;
    const extra = Object.entries(this.fields).map(([f, t]) => `, ${f} ${t} @hash`).join('');
    const quant = this.quant ? `, quant=${this.quant}` : '';
    const content = this.fullText ? 'content text @text' : 'content text';
    await this.client.run(
      `create collection if not exists ${this.collection} (lc_id text @hash, ${content}, metadata text, ` +
        `embedding vector<${dimension}> @hnsw(${this.metric}${quant})${extra})`,
      [],
    );
    // A collection made before without it takes the index now.
    if (this.fullText) await this.client.run(`create index if not exists on ${this.collection} (content) @text`, []);
    this.dimension = dimension;
  }

  /** A statement's rows, over a collection that may not be there yet: a
   * store nothing was written to is an empty one, not an error. */
  async #run(sql, params) {
    try {
      const out = await this.client.run(sql, params);
      return out?.rows ?? [];
    } catch (e) {
      // The page says `not found: collection `x``; the HTTP endpoint's 404
      // says `collection `x`` alone.
      if (/^(not found: )?collection `[^`]*`$/.test(String(e?.message))) return [];
      throw e;
    }
  }

  /** ` where f = $n and g in [$m, ...]` over the indexed fields. */
  #where(filter, first) {
    if (!filter || !Object.keys(filter).length) return ['', []];
    const terms = [];
    const params = [];
    for (let [name, want] of Object.entries(filter)) {
      if (!(name in this.fields)) {
        throw new Error(`${name} is not in metadataFields: only those can be filtered on`);
      }
      if (want && typeof want === 'object' && !Array.isArray(want)) {
        const keys = Object.keys(want);
        if (keys.length === 1 && keys[0] === '$eq') want = want.$eq;
        else if (keys.length === 1 && keys[0] === '$in') want = [...want.$in];
        else throw new Error(`${name}: only $eq and $in are supported`);
      }
      if (Array.isArray(want)) {
        terms.push(`${name} in [${placeholders(first + params.length, want.length)}]`);
        params.push(...want);
      } else {
        terms.push(`${name} = $${first + params.length}`);
        params.push(want);
      }
    }
    return [` where ${terms.join(' and ')}`, params];
  }

  #document(row) {
    return new Document({ id: row.lc_id, pageContent: row.content, metadata: JSON.parse(row.metadata || '{}') });
  }
}
