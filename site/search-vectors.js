/* The search by meaning: which model embeds the sections and the queries,
   and what of a section it reads. Shared by the build (`search-embed.mjs`),
   the Worker that embeds a reader's query (`worker.js`) and the page, so
   the three cannot drift apart: a vector from another model, or of another
   text, is a vector in another space. */

/* BAAI's bge-m3 on Workers AI: multilingual, so a question typed in Turkish
   finds the English page, and the cheapest embedding model there per token.
   Its vectors are 1024 wide and kept as f16 in the image. */
export const EMBED_MODEL = '@cf/baai/bge-m3';
export const EMBED_DIM = 1024;

/* A query is cut to this many characters before anything is asked of it,
   in the page and again in the Worker. */
export const MAX_QUERY = 200;

/* What a section is embedded as: where it is and the start of its text. A
   model reads 512 tokens or so well, and a section's opening says what it
   is about -- most of the long ones are tables and code after it. */
export function embedText(doc) {
  return `${doc.title}: ${doc.heading}\n${doc.body}`.slice(0, 2000);
}

/* The query as it is embedded and cached: one spelling for one question,
   so the same words from two readers are one cache entry. Cased letters
   fold the way the text index folds them, İ and I onto i. */
export function normalQuery(q) {
  return String(q)
    .normalize('NFC')
    .replace(/[İI]/g, 'i')
    .toLowerCase()
    .replace(/\s+/g, ' ')
    .trim()
    .slice(0, MAX_QUERY);
}
