// Tables declared in code, every column kind among them, for schema.ts to
// hold `$inferSelect` and `$inferInsert` to what `fenec types` generates for
// the same collections (schema-generated.d.ts). web/fenec.schema.test.js
// makes a file from these tables and runs `fenec types` over it, which must
// give that file again; `node check.mjs --schema` writes it.

import {
  fenecTable,
  text,
  integer,
  doublePrecision,
  boolean,
  timestamp,
  bytea,
  json,
  vector,
  halfvec,
  sparsevec,
  index,
  uniqueIndex,
  defineRelations,
} from '../schema.js';

export const articles = fenecTable(
  'articles',
  {
    title: text().notNull(),
    body: text().collate('und'),
    year: integer(),
    score: doublePrecision(),
    draft: boolean().notNull(),
    published: timestamp(),
    cover: bytea(),
    meta: json(),
    tags: text().array(),
    counts: integer().array().notNull(),
    embed: vector({ dimensions: 3 }),
    small: halfvec({ dimensions: 2 }),
    splade: sparsevec({ dimensions: 100 }),
  },
  (t) => [
    index('articles_body').using('bm25', t.body).with({ prefix: 6 }),
    index('articles_year').using('hash', t.year),
    index('articles_published').on(t.published),
    index('articles_embed').using('hnsw', t.embed.op('vector_cosine_ops')).with({ m: 8 }),
    index('articles_splade').using('inverted', t.splade),
    uniqueIndex('articles_lang').on(t.meta.path('lang')),
  ],
);

export const reviews = fenecTable(
  'reviews',
  { articleId: integer().notNull(), stars: integer(), note: text() },
  (t) => [index('reviews_article').using('hash', t.articleId)],
);

export const tables = { articles, reviews };

export const relations = defineRelations(tables, (r) => ({
  articles: { reviews: r.many.reviews({ from: r.articles.id, to: r.reviews.articleId }) },
  reviews: { article: r.one.articles({ from: r.reviews.articleId, to: r.articles.id }) },
}));
