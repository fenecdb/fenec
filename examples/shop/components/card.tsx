// A product in a grid. Shared by the server's pages and the "load more"
// island, so a card drawn in the browser is the card the server drew.
import { ProductArt } from './art';
import { Marked } from './marked';
import { money } from '../lib/money';
import { CATEGORY_SHAPES } from '../lib/category-shapes';

export interface CardData {
  sku: string;
  slug: string;
  name: string;
  brand: string;
  category: string;
  price: number;
  colour: string | null;
  rating: number | null;
  reviews: number | null;
  marks?: [number, number][];
  snippet?: { text: string; marks: [number, number][] } | null;
}

export function Stars({ rating, reviews }: { rating: number | null; reviews: number | null }) {
  if (!rating || !reviews) return <span className="stars none">No reviews yet</span>;
  return (
    <span className="stars" role="img" aria-label={`Rated ${rating.toFixed(1)} out of 5 from ${reviews} ${reviews === 1 ? 'review' : 'reviews'}`}>
      <span className="stars-bar" style={{ ['--r' as string]: rating / 5 }} aria-hidden="true" />
      <span aria-hidden="true">
        {rating.toFixed(1)} <span className="muted">({reviews})</span>
      </span>
    </span>
  );
}

export function ProductCard({ p, eager }: { p: CardData; eager?: boolean }) {
  // The brand is the start of the name; it is set apart on the card.
  const rest = p.name.startsWith(`${p.brand} `) ? p.name.slice(p.brand.length + 1) : p.name;
  const shift = p.name.length - rest.length;
  const marks = p.marks?.map(([a, b]) => [a - shift, b - shift] as [number, number]).filter(([, b]) => b > 0);
  return (
    <li className="card">
      <a href={`/p/${p.slug}`} className="card-link">
        <ProductArt shape={CATEGORY_SHAPES[p.category] ?? 'cube'} colour={p.colour} sku={p.sku} className={eager ? 'art' : 'art lazy'} />
        <span className="card-brand">{p.brand}</span>
        <span className="card-name">{marks?.length ? <Marked text={rest} marks={marks} /> : rest}</span>
      </a>
      {p.snippet && (
        <p className="card-snippet">
          <Marked text={p.snippet.text} marks={p.snippet.marks} />
        </p>
      )}
      <span className="card-foot">
        <span className="price">{money(p.price)}</span>
        <Stars rating={p.rating} reviews={p.reviews} />
      </span>
    </li>
  );
}
