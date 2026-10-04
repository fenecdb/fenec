/* The site's search: what it asks the database, and how the answers become
   one list. The dialog (`search.js`) and the build's test
   (`search.test.mjs`) both go through `search`, so the ranking the build
   holds to its known queries is the one a visitor gets.

   A section is a document of its own: the page's title, the heading, where
   it is, its text, the group of the docs it is under and what kind of page
   it is. Both texts are indexed with their prefixes, so a word being typed
   finds the words it begins -- `compa` finds compaction -- and an inflected
   word its stem, with no dictionary. The tokenizer folds İ, I, ı and i onto
   one letter, so a query typed on a Turkish keyboard finds the English
   text. */

export const SCHEMA =
  'create collection docs (title text, heading text @text(prefix=12), url text, ' +
  'body text @text(prefix=12), section text @hash, kind text @hash)';

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

/**
 * The best `limit` sections for `text`, under `section` when one is given,
 * and how many sections of each group hold a word of it.
 *
 * Every value goes in as a parameter: the query is the visitor's.
 * @returns {{hits: object[], facets: {value: string, count: number}[]}}
 */
export function search(db, text, section = null, limit = 10) {
  const q = text.trim();
  if (!q) return { hits: [], facets: [] };
  const where = section ? ' where section = $2' : '';
  const params = section ? [q, section] : [q];
  let body, heading;
  try {
    body = db.run(`get docs select id${where} match body $1 limit ${DEPTH}${section ? '' : ' facet section'}`, params);
    heading = db.run(`get docs select id${where} match heading $1 limit ${DEPTH}`, params);
  } catch {
    // A query of nothing but punctuation holds no word to match.
    return { hits: [], facets: [] };
  }
  // The counts are over every group, so the chips keep their numbers while
  // one of them narrows the list: under one, they are asked apart.
  const counted = section ? db.run('get docs match body $1 limit 0 facet section', [q]) : body;
  const facets = counted.facets?.section ?? [];

  const score = new Map();
  const inBody = new Set();
  for (const r of body.rows) {
    score.set(r.id, r._score);
    inBody.add(r.id);
  }
  for (const r of heading.rows) score.set(r.id, (score.get(r.id) ?? 0) + HEADING_WEIGHT * r._score);
  const best = [...score].sort((a, b) => b[1] - a[1] || a[0] - b[0]).slice(0, limit).map(([id]) => id);

  // The rows shown, marked: those whose text holds a word of the query
  // through the text's `match`, the rest -- found by their heading alone --
  // through the heading's, which marks no word of the text and starts the
  // snippet at its beginning.
  const rows = new Map();
  const shown = (field, ids) => {
    if (!ids.length) return;
    const list = ids.map((_, i) => `$${i + 2}`).join(', ');
    const got = db.run(`get docs select ${COLUMNS} where id in [${list}] match ${field} $1 limit ${ids.length}`, [q, ...ids]);
    for (const r of got.rows) rows.set(r.id, r);
  };
  shown('body', best.filter((id) => inBody.has(id)));
  shown('heading', best.filter((id) => !inBody.has(id)));

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
      snippet: r['snippet(body)'] ?? { text: '', marks: [] },
    };
  });
  return { hits, facets };
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
