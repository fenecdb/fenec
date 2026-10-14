// The site's search index, written by the engine the site ships.
//
//   node site/search-index.mjs <fenec.wasm> <out> <out-vectors> <vectors.json> < documents.json
//
// `build.py` cuts every page into its sections and hands them over as JSON;
// this writes them into a database through the browser module itself and
// the database's image to <out>, which the page loads with `load` -- the
// same bytes, read by the same code, so what the build tests is what a
// visitor searches.
//
// Two images of the same documents: <out> with their words, which the
// search opens with, and <out-vectors> with their embeddings as well
// (<vectors.json>, search-embed.mjs's: a vector a document, or null for
// one with none), which the page loads only once the search by meaning
// answers it -- the vectors are three quarters of its bytes, and a reader
// whose endpoint is off never pays for them. It prints how many documents
// each image holds.

import { readFileSync, writeFileSync } from 'node:fs';
import { Fenec } from '../web/fenec.js';
import { SCHEMA, SCHEMA_VECTORS } from './search-query.js';

const [wasm, out, outVectors, vectorsFile] = process.argv.slice(2);
const docs = JSON.parse(readFileSync(0, 'utf8'));
const vectors = JSON.parse(readFileSync(vectorsFile, 'utf8'));
if (vectors.length !== docs.length) throw new Error(`${vectors.length} vectors for ${docs.length} documents`);
const module = readFileSync(wasm);

async function image(schema, path, withVectors) {
  const db = await Fenec.open(module);
  db.run(schema);
  // One statement a section, every value a parameter: the text is the
  // pages', and none of it is FenecQL.
  const put = withVectors
    ? 'put docs {title: $1, heading: $2, url: $3, body: $4, section: $5, kind: $6, embed: $7}'
    : 'put docs {title: $1, heading: $2, url: $3, body: $4, section: $5, kind: $6}';
  docs.forEach((d, i) => {
    const values = [d.title, d.heading, d.url, d.body, d.section, d.kind];
    if (withVectors) values.push(vectors[i] && Float32Array.from(vectors[i]));
    db.run(put, values);
  });
  writeFileSync(path, db.snapshot());
  return db.run('get docs count').rows[0].count;
}

const words = await image(SCHEMA, out, false);
const meaning = await image(SCHEMA_VECTORS, outVectors, true);
process.stdout.write(`${words} ${meaning}\n`);
