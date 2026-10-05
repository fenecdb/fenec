// A product's attributes as a vector, for "more like this". It is not a
// language model's embedding -- nothing in the shop can make one without a
// download or a key -- so it knows nothing of meaning: two products are
// near when they share a category, a colour and a material, and cost and
// rate alike. That is what a "related products" strip wants anyway. A
// semantic search would put a model's vector beside it (see the README's
// "Semantic search" and lib/catalog.ts's `searchHook`).
import { CATEGORIES, COLOURS } from './catalog-data';
import { FEATURE_DIMENSIONS } from './schema';

const MATERIAL_BUCKETS = 4;

function fnv(s: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 0x01000193) >>> 0;
  return h;
}

export function featuresOf(p: { category: string; colour: string | null; material: string | null; price: number; rating: number | null }): number[] {
  const v = new Array<number>(FEATURE_DIMENSIONS).fill(0);
  const c = CATEGORIES.findIndex((x) => x.slug === p.category);
  if (c >= 0) v[c] = 1;
  const k = p.colour ? COLOURS.indexOf(p.colour) : -1;
  if (k >= 0) v[30 + k] = 0.45;
  if (p.material) v[42 + (fnv(p.material) % MATERIAL_BUCKETS)] = 0.35;
  // log price over $1..$1000, and the rating out of five
  v[46] = 0.5 * Math.min(1, Math.max(0, Math.log10(p.price / 100) / 3));
  v[47] = 0.25 * ((p.rating ?? 4) / 5);
  const norm = Math.hypot(...v);
  return v.map((x) => Math.round((x / norm) * 1e6) / 1e6);
}
