/* The site's search: what it asks the database, and how the answers become
   one list. The dialog (`search.js`) and the build's test
   (`search.test.mjs`) both go through `search`, so the ranking the build
   holds to its known queries is the one a visitor gets.

   A section is a document of its own: the page's title, the heading, where
   it is, its text, the group of the docs it is under and what kind of page
   it is, and the embedding of its opening (search-vectors.js). Both texts
   are indexed with their prefixes, so a word being typed finds the words it
   begins -- `compa` finds compaction -- and an inflected word its stem, with
   no dictionary. The tokenizer folds İ, I, ı and i onto one letter, so a
   query typed on a Turkish keyboard finds the English text. */

import { EMBED_DIM } from './search-vectors.js';

const FIELDS =
  'title text, heading text @text(prefix=12), url text, body text @text(prefix=12), ' +
  'section text @hash, kind text @hash';

/* The image the dialog opens with, words alone, and the one it loads once
   the search by meaning answers: the same documents with their vectors.
   f16, since an f32 doubles the image for nothing a ranking of 282
   sections can tell; no quantization, since the codes are derived at the
   load and the documents keep their f16s either way -- int8 and bit codes
   left the image the size it was and the search no faster. */
export const SCHEMA = `create collection docs (${FIELDS})`;
export const SCHEMA_VECTORS = `create collection docs (${FIELDS}, embed vector<${EMBED_DIM}, f16> @hnsw(cosine))`;

/* FenecQL has no weight for a field, and `match` searches one. So the
   heading is searched apart and its score counts this many times over the
   text's: a section named for a word outranks one that only mentions it,
   which is what a reader who types the word is after. */
export const HEADING_WEIGHT = 3;

/* Each search reads this deep a side before the two are added up; a section
   found by one alone keeps its one score. */
const DEPTH = 40;

/* What a hit shows. A snippet reads its row's text again, which is most of
   a query's time over sections thousands of words long -- asked of 40 rows
   a side it took 4 to 8 ms in Node, where the ranking took 0.1 -- so the
   ranking reads ids and scores alone, and the marks are asked of the rows
   shown. */
const COLUMNS =
  'id, title, heading, url, section, kind, highlight(title), highlight(heading), snippet(body, 26, "…")';
const SNIPPET_WORDS = 26;

/**
 * The best `limit` sections for `text`, under `section` when one is given,
 * and how many sections of each group hold a word of it.
 *
 * With `vector`, the query's embedding, the words and the meaning rank
 * together: `match body ... near embed ... fuse`, the engine adding the
 * two lists' ranks. The heading's own match stays out of it: a third list
 * outvoted the meaning -- over 14 questions worded unlike the docs, the
 * right page came first for 11 without it and for 6 with it beside the
 * two at full weight; the words alone, 7.
 *
 * Every value goes in as a parameter: the query is the visitor's.
 * @returns {{hits: object[], facets: {value: string, count: number}[]}}
 */
export function search(db, text, section = null, limit = 10, vector = null) {
  const q = text.trim();
  if (!q) return { hits: [], facets: [] };
  const where = section ? ' where section = $2' : '';
  const params = section ? [q, section] : [q];
  const v = `$${params.length + 1}`;
  let body, ranked;
  try {
    body = db.run(`get docs select id${where} match body $1 limit ${DEPTH}${section ? '' : ' facet section'}`, params);
    ranked = vector
      ? db.run(`get docs select id${where} match body $1 near embed ${v} fuse candidates ${DEPTH} limit ${DEPTH}`,
        [...params, Float32Array.from(vector)]).rows
      : null;
  } catch {
    // A query of nothing but punctuation holds no word to match.
    return { hits: [], facets: [] };
  }
  // The counts are over every group, so the chips keep their numbers while
  // one of them narrows the list: under one, they are asked apart. `facet`
  // goes with `match` and not with `near`, which ranks every row rather than
  // choosing some, so they count the sections holding a word of the query.
  const counted = section ? db.run('get docs match body $1 limit 0 facet section', [q]) : body;
  const facets = counted.facets?.section ?? [];

  const inBody = new Set(body.rows.map((r) => r.id));
  const score = new Map();
  if (ranked) {
    for (const r of ranked) score.set(r.id, r._score);
  } else {
    const heading = db.run(`get docs select id${where} match heading $1 limit ${DEPTH}`, params);
    for (const r of body.rows) score.set(r.id, r._score);
    for (const r of heading.rows) score.set(r.id, (score.get(r.id) ?? 0) + HEADING_WEIGHT * r._score);
  }
  const best = [...score].sort((a, b) => b[1] - a[1] || a[0] - b[0]).slice(0, limit).map(([id]) => id);

  // The rows shown, marked: those whose text holds a word of the query
  // through the text's `match`; those found by their heading alone through
  // the heading's, which marks no word of the text and starts the snippet at
  // its beginning; and those found by their meaning alone, which hold no
  // word of the query to mark, with the opening of their text.
  const rows = new Map();
  const shown = (how, ids) => {
    if (!ids.length) return;
    const list = (from) => ids.map((_, i) => `$${i + from}`).join(', ');
    const got = how === 'meaning'
      ? db.run(`get docs select id, title, heading, url, section, kind, body where id in [${list(1)}] limit ${ids.length}`, ids)
      : db.run(`get docs select ${COLUMNS} where id in [${list(2)}] match ${how} $1 limit ${ids.length}`, [q, ...ids]);
    for (const r of got.rows) rows.set(r.id, r);
  };
  shown('body', best.filter((id) => inBody.has(id)));
  if (ranked) shown('meaning', best.filter((id) => !inBody.has(id)));
  else shown('heading', best.filter((id) => !inBody.has(id)));

  const hits = best.filter((id) => rows.has(id)).map((id) => {
    const r = rows.get(id);
    return {
      id,
      title: r.title,
      heading: r.heading,
      url: r.url,
      page: r.url.split('#')[0],
      section: r.section,
      kind: r.kind,
      score: score.get(id),
      titleMarks: r['highlight(title)'] ?? [],
      headingMarks: r['highlight(heading)'] ?? [],
      snippet: r['snippet(body)'] ?? opening(r.body),
      byMeaning: Boolean(ranked) && !inBody.has(id),
    };
  });
  return { hits, facets };
}

/* The first words of a text, as a snippet with nothing marked. */
function opening(text = '') {
  const words = text.split(/\s+/).filter(Boolean);
  const cut = words.length > SNIPPET_WORDS;
  return { text: words.slice(0, SNIPPET_WORDS).join(' ') + (cut ? '…' : ''), marks: [] };
}

/**
 * The hits grouped by page, each page where its best section stands and its
 * sections in their order: what the dialog shows, top to bottom.
 */
export function grouped(hits) {
  const pages = new Map();
  for (const h of hits) {
    if (!pages.has(h.page)) pages.set(h.page, { page: h.page, title: h.title, titleMarks: h.titleMarks, hits: [] });
    pages.get(h.page).hits.push(h);
  }
  return [...pages.values()];
}
