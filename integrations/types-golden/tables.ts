// generated from the fenecdb schema: schema.fenecql
// fenec types --schema schema.fenecql > schema.ts
// The tables are the code's from here on: change them here, and an open
// with them checks the database against them (Fenec.open's `schema`).

import { fenecTable, text, integer, doublePrecision, boolean, timestamp, bytea, json, vector, halfvec, sparsevec, index } from '@fenecdb/web/schema';

export const articles = fenecTable('articles', {
  title: text().collate('tr').notNull(),
  year: integer(),
  score: doublePrecision(),
  draft: boolean().notNull(),
  published: timestamp(),
  cover: bytea(),
  meta: json(),
  loc: geometry({ type: 'point' }),
  embed: vector({ dimensions: 384 }),
  small: halfvec({ dimensions: 4 }),
  splade: sparsevec({ dimensions: 30522 }),
  tags: text().array(),
  counts: integer().array().notNull(),
  slug: text().unique(),
}, (t) => [
  index('articles_title_idx').using('bm25', t.title).with({ prefix: 6, chars: true }),
  index('articles_year_idx').using('hash', t.year),
  index('articles_score_idx').on(t.score),
  index('articles_published_idx').on(t.published).ttl('30d'),
  index('articles_loc_idx').using('gist', t.loc),
  index('articles_embed_idx').using('hnsw', t.embed.op('vector_cosine_ops')).with({ m: 8 }),
  index('articles_small_idx').using('hnsw', t.small.op('halfvec_l2_ops')).with({ quant: 'int8' }),
  index('articles_splade_idx').using('inverted', t.splade),
  index('articles_tags_idx').using('hash', t.tags),
  index('articles_meta_lang_idx').using('hash', t.meta.path('lang')),
  index('articles_meta_source_rank_idx').on(t.meta.path('source.rank')),
]);

export const product_reviews = fenecTable('product_reviews', {
  product_id: integer().notNull(),
  stars: integer(),
  notes: text().collate('und'),
}, (t) => [
  index('product_reviews_product_id_idx').using('hash', t.product_id),
]);

export const ki_iler = fenecTable('kişiler', {
  ad: text(),
  'yaş': integer(),
});

export const textTable = fenecTable('text', {
  body: text(),
});
