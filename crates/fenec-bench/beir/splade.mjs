// Encodes a BEIR dataset with SPLADE for the sparse arm of `make beir`.
//
//   npm install && node splade.mjs <dataset dir>
//
// The model is SPLADE++ (prithivida/Splade_PP_en_v1: BERT-base, Apache 2.0,
// 532 MB of ONNX), run on the CPU. SPLADE gives a text a weight for every
// entry of the vocabulary -- the masked-language-model logits, through
// log(1 + relu(x)), the largest over the text's tokens -- and all but a few
// hundred of the 30 522 come out zero. The model learns which words a text
// is about, including ones it never uses, so the vector is served the way a
// word index is and scored by a dot product.
//
// The ONNX file names its inputs as BERT's TensorFlow export did
// (`input_mask`, `segment_ids`), which transformers.js does not map, so the
// tokenizer comes from transformers.js and the model runs on the ONNX
// runtime directly. CoreML refused the model on an M1 and WebGPU ran it at
// 3.3 texts a second against the CPU's 5.3: the 30 522 logits a token have
// to come back to be pooled either way. Texts go through sorted by length,
// so a batch is padded to about its own length rather than to 256.
//
// Writes, beside the dataset, in the order of corpus.ids / queries.ids (the
// order embed.mjs writes them in):
//   corpus.sparse / queries.sparse   per text a little-endian u32 count, that
//                                    many u32 vocabulary ids ascending, then
//                                    their f32 weights

import { AutoTokenizer } from '@huggingface/transformers';
import ort from 'onnxruntime-node';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const dir = process.argv[2];
if (!dir) {
  console.error('usage: node splade.mjs <BEIR dataset dir>');
  process.exit(2);
}
const MODEL = 'prithivida/Splade_PP_en_v1';
const MAX_TOKENS = 256;
const BATCH = 8;

const tokenizer = await AutoTokenizer.from_pretrained(MODEL);
// transformers.js keeps what it fetched under its own cache; the tokenizer
// above put the model's small files there, and the ONNX file is fetched into
// the same place the first time.
const here = path.dirname(fileURLToPath(import.meta.url));
const cached = path.join(here, 'node_modules/@huggingface/transformers/.cache', MODEL, 'onnx/model.onnx');
if (!fs.existsSync(cached)) {
  fs.mkdirSync(path.dirname(cached), { recursive: true });
  process.stderr.write(`fetching ${MODEL}/onnx/model.onnx (532 MB)\n`);
  const res = await fetch(`https://huggingface.co/${MODEL}/resolve/main/onnx/model.onnx`);
  if (!res.ok) throw new Error(`model.onnx: HTTP ${res.status}`);
  fs.writeFileSync(`${cached}.part`, Buffer.from(await res.arrayBuffer()));
  fs.renameSync(`${cached}.part`, cached);
}
const session = await ort.InferenceSession.create(cached, { executionProviders: ['cpu'] });

const lines = (file) =>
  fs.readFileSync(path.join(dir, file), 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l));

/// The ids file embed.mjs writes, or this order written as one when it has
/// not run: the two arms must agree on which vector is which document.
function ids(file, want) {
  const at = path.join(dir, file);
  if (!fs.existsSync(at)) {
    fs.writeFileSync(at, want.join('\n') + '\n');
    return;
  }
  const have = fs.readFileSync(at, 'utf8').split('\n').filter(Boolean);
  if (have.length !== want.length || have.some((id, i) => id !== want[i])) {
    throw new Error(`${file} lists other ids than the dataset in its order; delete it and rerun`);
  }
}

/// A batch as the model takes it, each text cut to `MAX_TOKENS` here rather
/// than by transformers.js: cutting one, it drops the closing [SEP], and
/// SPLADE without it weighs a long text's vocabulary at a fraction of what
/// it should -- 28 non-zero entries for a SciFact abstract that has 219 --
/// which took SciFact's nDCG@10 to 0.23.
async function batch(texts) {
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
  return { n, len, ids, mask };
}

async function encode(texts, name) {
  const order = texts.map((_, i) => i).sort((a, b) => texts[b].length - texts[a].length);
  const out = new Array(texts.length);
  const t0 = Date.now();
  let nnz = 0;
  for (let i = 0; i < order.length; i += BATCH) {
    const idx = order.slice(i, i + BATCH);
    const { n, len, ids, mask } = await batch(idx.map((j) => texts[j]));
    const feeds = {
      input_ids: new ort.Tensor('int64', ids, [n, len]),
      input_mask: new ort.Tensor('int64', mask, [n, len]),
      segment_ids: new ort.Tensor('int64', new BigInt64Array(n * len), [n, len]),
    };
    const { output } = await session.run(feeds);
    const vocab = output.dims[2];
    const logits = output.data;
    for (let b = 0; b < n; b++) {
      const w = new Float32Array(vocab);
      for (let t = 0; t < len; t++) {
        if (!Number(mask[b * len + t])) continue;
        const row = (b * len + t) * vocab;
        for (let v = 0; v < vocab; v++) {
          const x = logits[row + v];
          if (x > 0) {
            const y = Math.log1p(x);
            if (y > w[v]) w[v] = y;
          }
        }
      }
      const keep = [];
      for (let v = 0; v < vocab; v++) if (w[v] > 0) keep.push(v);
      const rec = Buffer.alloc(4 + keep.length * 8);
      rec.writeUInt32LE(keep.length, 0);
      keep.forEach((v, k) => {
        rec.writeUInt32LE(v, 4 + k * 4);
        rec.writeFloatLE(w[v], 4 + keep.length * 4 + k * 4);
      });
      out[idx[b]] = rec;
      nnz += keep.length;
    }
    if ((i / BATCH) % 25 === 0) {
      const done = i + n;
      const rate = done / ((Date.now() - t0) / 1000);
      process.stderr.write(`${name}: ${done}/${texts.length}, ${rate.toFixed(1)}/s\r`);
    }
  }
  fs.writeFileSync(path.join(dir, `${name}.sparse`), Buffer.concat(out));
  const secs = (Date.now() - t0) / 1000;
  process.stderr.write(
    `${name}: ${texts.length} in ${secs.toFixed(1)} s, ${(nnz / texts.length).toFixed(1)} non-zero a text\n`,
  );
}

const corpus = lines('corpus.jsonl');
ids('corpus.ids', corpus.map((d) => d._id));
const wanted = new Set(
  fs.readFileSync(path.join(dir, 'qrels/test.tsv'), 'utf8').split('\n').slice(1).filter(Boolean)
    .map((l) => l.split('\t')[0]),
);
const queries = lines('queries.jsonl').filter((q) => wanted.has(q._id));
ids('queries.ids', queries.map((q) => q._id));
await encode(queries.map((q) => q.text), 'queries');
await encode(corpus.map((d) => `${d.title ?? ''} ${d.text ?? ''}`.trim()), 'corpus');
