// generated from the fenecdb schema: tables.fenec
// fenec types tables.fenec > fenec-schema.d.ts
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
    /** text required */
    title: string;
    /** text @text(k1=0.9, b=0.4, prefix=6) */
    body: string | null;
    /** int @hash */
    year: number | null;
    /** float */
    score: number | null;
    /** bool required */
    draft: boolean;
    /** timestamp @sorted */
    published: Timestamp | null;
    /** bytes */
    cover: Bytes | null;
    /** json */
    meta: Json | null;
    /** [text] */
    tags: string[] | null;
    /** [int] required */
    counts: number[];
    /** vector<3> @hnsw(cosine, m=8, ef_search=100) */
    embed: Vector | null;
    /** vector<2, f16> */
    small: Vector | null;
    /** sparse<100> @inverted */
    splade: Sparse | null;
  };

  reviews: {
    /** int @hash required */
    articleId: number;
    /** int */
    stars: number | null;
    /** text */
    note: string | null;
  };
};
