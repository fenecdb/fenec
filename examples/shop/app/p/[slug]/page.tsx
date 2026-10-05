import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { category, fromBrand, product, related, stockOf } from '../../../lib/catalog';
import { ProductArt } from '../../../components/art';
import { ProductCard, Stars } from '../../../components/card';
import { Breadcrumbs, JsonLd, SITE } from '../../../components/seo';
import { LiveStock } from '../../../components/live-stock';
import { CATEGORY_SHAPES } from '../../../lib/category-shapes';
import { decimal, money } from '../../../lib/money';
import { MAX_QTY } from '../../../lib/cart-limits';

// Each product page is made the first time it is asked for, then kept and
// made again in the background at most every five minutes. The stock it
// shows is that moment's; the island below brings it up to date.
export const revalidate = 300;
export const dynamicParams = true;
export async function generateStaticParams() {
  return [];
}

type Props = { params: Promise<{ slug: string }> };

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { slug } = await params;
  const p = await product(slug);
  if (!p) return {};
  const description = p.description.length > 155 ? `${p.description.slice(0, 152).replace(/\s+\S*$/, '')}…` : p.description;
  return {
    title: p.name,
    description,
    alternates: { canonical: `/p/${p.slug}` },
    openGraph: { title: p.name, description, url: `/p/${p.slug}`, type: 'website', images: [{ url: `/img/p/${p.sku}.svg`, width: 1200, height: 1200, alt: p.name }] },
  };
}

function grams(g: number | null) {
  if (!g) return '';
  return g >= 1000 ? `${(g / 1000).toLocaleString('en-US', { maximumFractionDigits: 2 })} kg` : `${g} g`;
}

export default async function ProductPage({ params }: Props) {
  const { slug } = await params;
  const p = await product(slug);
  if (!p) notFound();
  const [c, stock, like, brand] = await Promise.all([category(p.category), stockOf(p.sku), related(p), fromBrand(p)]);
  const shape = CATEGORY_SHAPES[p.category] ?? 'cube';
  const url = `${SITE}/p/${p.slug}`;
  return (
    <>
      <Breadcrumbs
        trail={[
          { name: 'Home', href: '/' },
          { name: c?.name ?? p.category, href: `/c/${p.category}` },
          { name: p.name, href: `/p/${p.slug}` },
        ]}
      />
      <JsonLd
        data={{
          '@context': 'https://schema.org',
          '@type': 'Product',
          name: p.name,
          sku: p.sku,
          description: p.description,
          image: `${SITE}/img/p/${p.sku}.svg`,
          brand: { '@type': 'Brand', name: p.brand },
          category: c?.name,
          color: p.colour ?? undefined,
          material: p.material ?? undefined,
          weight: p.weight ? { '@type': 'QuantitativeValue', value: p.weight, unitCode: 'GRM' } : undefined,
          offers: {
            '@type': 'Offer',
            url,
            price: decimal(p.price),
            priceCurrency: 'USD',
            availability: stock > 0 ? 'https://schema.org/InStock' : 'https://schema.org/OutOfStock',
            itemCondition: 'https://schema.org/NewCondition',
          },
          ...(p.rating && p.reviews
            ? { aggregateRating: { '@type': 'AggregateRating', ratingValue: p.rating, reviewCount: p.reviews, bestRating: 5, worstRating: 1 } }
            : {}),
        }}
      />
      <article className="product">
        <ProductArt shape={shape} colour={p.colour} sku={p.sku} label={`Drawing of the ${p.name}`} />
        <div>
          <p className="product-brand">{p.brand}</p>
          <h1>{p.name.startsWith(`${p.brand} `) ? p.name.slice(p.brand.length + 1) : p.name}</h1>
          <Stars rating={p.rating} reviews={p.reviews} />
          <div className="buy">
            <span className="buy-price">{money(p.price)}</span>
            <LiveStock sku={p.sku} initial={stock} />
            <form action="/api/cart" method="post">
              <input type="hidden" name="action" value="add" />
              <input type="hidden" name="sku" value={p.sku} />
              <label htmlFor="qty" className="sr">Quantity</label>
              <select id="qty" name="qty" className="qty" defaultValue="1">
                {Array.from({ length: Math.min(10, MAX_QTY) }, (_, i) => (
                  <option key={i + 1} value={i + 1}>{i + 1}</option>
                ))}
              </select>
              <button className="button" type="submit" disabled={stock <= 0}>{stock > 0 ? 'Add to cart' : 'Sold out'}</button>
            </form>
            <p className="muted" style={{ margin: 0 }}>Free shipping over $150. Returns within 60 days.</p>
          </div>
          <p>{p.description}</p>
          <dl className="specs">
            {p.colour && (<><dt>Colour</dt><dd>{p.colour}</dd></>)}
            {p.size && (<><dt>Size</dt><dd>{p.size}</dd></>)}
            {p.material && (<><dt>Material</dt><dd>{p.material}</dd></>)}
            {p.weight && (<><dt>Weight</dt><dd>{grams(p.weight)}</dd></>)}
            <dt>Item</dt><dd>{p.sku}</dd>
          </dl>
        </div>
      </article>
      {like.length > 0 && (
        <section className="section" aria-labelledby="like">
          <h2 id="like">Close alternatives</h2>
          <ul className="grid">{like.map((r) => <ProductCard key={r.sku} p={r} />)}</ul>
        </section>
      )}
      {brand.length > 0 && (
        <section className="section" aria-labelledby="brand">
          <h2 id="brand">More from {p.brand}</h2>
          <ul className="grid">{brand.map((r) => <ProductCard key={r.sku} p={r} />)}</ul>
        </section>
      )}
    </>
  );
}
