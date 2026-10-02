// generated from the fenecdb schema: schema.fenecql
// fenec types schema.fenecql > fenec-schema.d.ts
// Do not edit by hand -- regenerate when the schema changes.

/** `timestamp`: ISO-8601 text when read; a Date/number is accepted when writing. */
export type Timestamp = string & { readonly __fenec: 'timestamp' };
/** `sparse<N>`: pgvector's text form, `{1:0.5,3:0.25}/N`, indices from 1. */
export type Sparse = string & { readonly __fenec: 'sparse' };
/** `vector<N>`: an array of numbers in JSON. */
export type Vector = number[] & { readonly __fenec: 'vector' };
/** `json`: any value JSON holds; a path reads into it, `'meta.lang'`. */
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
/** `bytes`: an array of bytes in JSON. */
export type Bytes = number[] & { readonly __fenec: 'bytes' };

export type FenecSchema = {
  articles: {
    /** text @text(k1=0.9, b=0.4, prefix=6, chars) required */
    title: string;
    /** int @hash */
    year: number | null;
    /** float @sorted */
    score: number | null;
    /** bool required */
    draft: boolean;
    /** timestamp @ttl(30d) */
    published: Timestamp | null;
    /** bytes */
    cover: Bytes | null;
    /** json */
    meta: Json | null;
    /** vector<384> @hnsw(cosine, m=8, ef_search=100) */
    embed: Vector | null;
    /** vector<4, f16> @hnsw(l2, m=16, ef_search=100, quant=int8) */
    small: Vector | null;
    /** sparse<30522> @inverted */
    splade: Sparse | null;
    /** [text] @hash */
    tags: string[] | null;
    /** [int] required */
    counts: number[];
    /** text @unique */
    slug: string | null;
  };

  product_reviews: {
    /** int @hash required */
    product_id: number;
    /** int */
    stars: number | null;
    /** text */
    notes: string | null;
  };

  'kişiler': {
    /** text */
    ad: string | null;
    /** int */
    'yaş': number | null;
  };

  text: {
    /** text */
    body: string | null;
  };
};
