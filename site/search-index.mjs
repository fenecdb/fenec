// The site's search index, written by the engine the site ships.
//
//   node site/search-index.mjs <fenec.wasm> <out> < documents.json
//
// `build.py` cuts every page into its sections and hands them over as JSON;
// this writes them into a database through the browser module itself and
// the database's image to <out>, which the page loads with `load` -- the
// same bytes, read by the same code, so what the build tests is what a
// visitor searches. It prints how many documents the image holds.

import { readFileSync, writeFileSync } from 'node:fs';
import { Fenec } from '../web/fenec.js';
import { SCHEMA } from './search-query.js';

const [wasm, out] = process.argv.slice(2);
const docs = JSON.parse(readFileSync(0, 'utf8'));

const db = await Fenec.open(readFileSync(wasm));
db.run(SCHEMA);
// One statement a section, every value a parameter: the text is the
// pages', and none of it is FenecQL.
const put = 'put docs {title: $1, heading: $2, url: $3, body: $4, section: $5, kind: $6}';
for (const d of docs) db.run(put, [d.title, d.heading, d.url, d.body, d.section, d.kind]);

const held = db.run('get docs count').rows[0].count;
writeFileSync(out, db.snapshot());
process.stdout.write(`${held}\n`);
