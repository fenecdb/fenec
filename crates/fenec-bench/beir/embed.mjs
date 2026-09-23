// Embeds a BEIR dataset for `make beir BEIR=<dataset dir>`.
//
//   npm install && node embed.mjs <dataset dir>
//
// The model is all-MiniLM-L6-v2 -- 384 dimensions, 90 MB, the one the BEIR
// numbers in the docs are quoted for -- run on the CPU through ONNX. It is
// done the way sentence-transformers does it, so the scores compare with
// the published ones: a document is its title and text, both truncated to
// 256 tokens, mean-pooled over the tokens and normalised.
//
// Writes, beside the dataset:
//   corpus.f32 / corpus.ids     one vector per document, its id per line
//   queries.f32 / queries.ids   the same for the queries the test qrels name

import { AutoModel, AutoTokenizer, Tensor } from '@huggingface/transformers';
import fs from 'node:fs';
import path from 'node:path';

const dir = process.argv[2];
if (!dir) {
  console.error('usage: node embed.mjs <BEIR dataset dir>');
  process.exit(2);
}
const MODEL = 'Xenova/all-MiniLM-L6-v2';
const MAX_TOKENS = 256;
const BATCH = 32;

const tokenizer = await AutoTokenizer.from_pretrained(MODEL);
const model = await AutoModel.from_pretrained(MODEL, { dtype: 'fp32' });

const lines = (file) =>
  fs.readFileSync(path.join(dir, file), 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l));

/// A batch as the model takes it, each text cut to `MAX_TOKENS` here rather
/// than by transformers.js: cutting one, it drops the closing [SEP], which
/// sentence-transformers keeps -- splade.mjs works around the same fault.
async function encode(texts) {
  const rows = [];
  for (const t of texts) {
    const { input_ids } = await tokenizer(t);
    let ids = Array.from(input_ids.data, Number);
    if (ids.length > MAX_TOKENS) {
      ids = [...ids.slice(0, MAX_TOKENS - 1), ids[ids.length - 1]];
    }
    rows.push(ids);
  }
  const n = rows.length;
  const len = Math.max(...rows.map((r) => r.length));
  const ids = new BigInt64Array(n * len);
  const mask = new BigInt64Array(n * len);
  rows.forEach((r, b) =>
    r.forEach((id, t) => {
      ids[b * len + t] = BigInt(id);
      mask[b * len + t] = 1n;
    }),
  );
  return {
    input_ids: new Tensor('int64', ids, [n, len]),
    attention_mask: new Tensor('int64', mask, [n, len]),
    token_type_ids: new Tensor('int64', new BigInt64Array(n * len), [n, len]),
  };
}

async function embed(texts, name) {
  const out = fs.openSync(path.join(dir, `${name}.f32`), 'w');
  const t0 = Date.now();
  for (let i = 0; i < texts.length; i += BATCH) {
    const batch = texts.slice(i, i + BATCH);
    const enc = await encode(batch);
    const { last_hidden_state: h } = await model(enc);
    const [n, len, dim] = h.dims;
    const mask = enc.attention_mask.data;
    const vec = new Float32Array(n * dim);
    for (let b = 0; b < n; b++) {
      let count = 0;
      for (let t = 0; t < len; t++) {
        if (!Number(mask[b * len + t])) continue;
        count++;
        for (let d = 0; d < dim; d++) vec[b * dim + d] += h.data[(b * len + t) * dim + d];
      }
      let norm = 0;
      for (let d = 0; d < dim; d++) {
        vec[b * dim + d] /= count;
        norm += vec[b * dim + d] ** 2;
      }
      norm = Math.sqrt(norm) || 1;
      for (let d = 0; d < dim; d++) vec[b * dim + d] /= norm;
    }
    fs.writeSync(out, Buffer.from(vec.buffer));
    if ((i / BATCH) % 50 === 0) {
      const rate = (i + n) / ((Date.now() - t0) / 1000);
      process.stderr.write(`${name}: ${i + n}/${texts.length}, ${rate.toFixed(0)}/s\r`);
    }
  }
  fs.closeSync(out);
  process.stderr.write(`${name}: ${texts.length} in ${((Date.now() - t0) / 1000).toFixed(1)} s\n`);
}

const corpus = lines('corpus.jsonl');
fs.writeFileSync(path.join(dir, 'corpus.ids'), corpus.map((d) => d._id).join('\n') + '\n');
await embed(corpus.map((d) => `${d.title ?? ''} ${d.text ?? ''}`.trim()), 'corpus');

const wanted = new Set(
  fs.readFileSync(path.join(dir, 'qrels/test.tsv'), 'utf8').split('\n').slice(1).filter(Boolean)
    .map((l) => l.split('\t')[0]),
);
const queries = lines('queries.jsonl').filter((q) => wanted.has(q._id));
fs.writeFileSync(path.join(dir, 'queries.ids'), queries.map((q) => q._id).join('\n') + '\n');
await embed(queries.map((q) => q.text), 'queries');
