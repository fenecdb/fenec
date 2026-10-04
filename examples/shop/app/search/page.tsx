import type { Metadata } from 'next';
import { categoryList, listing, readParams, PAGE_SIZE } from '../../lib/catalog';
import { FacetList, Results, hrefOf, type ListingState } from '../../components/listing';

type Props = { searchParams: Promise<Record<string, string | string[] | undefined>> };

export async function generateMetadata({ searchParams }: Props): Promise<Metadata> {
  const { q } = readParams(await searchParams);
  return {
    title: q ? `Search: ${q}` : 'Search',
    // Results pages are for shoppers; the products they lead to are what
    // a search engine should index.
    robots: { index: false, follow: true },
    alternates: { canonical: '/search' },
  };
}

export default async function SearchPage({ searchParams }: Props) {
  const { q, filters, page } = readParams(await searchParams);
  const s: ListingState = { base: '/search', filters, sort: 'featured', page, q };
  if (!q) {
    const cats = await categoryList();
    return (
      <>
        <h1>Search</h1>
        <p className="muted">Type what you are after in the box above: a kind of gear, a maker, a material or a colour.</p>
        <ul className="sorts">{cats.map((c) => <li key={c.slug}><a href={`/c/${c.slug}`}>{c.name}</a></li>)}</ul>
      </>
    );
  }
  const [{ cards, facets, total }, cats] = await Promise.all([listing({ q, filters, sort: 'featured', page }), categoryList()]);
  // The category facet answers slugs; the list shows their names.
  const label = new Map(cats.map((c) => [c.slug, c.name]));
  const from = (page - 1) * PAGE_SIZE + 1;
  return (
    <>
      <div className="listing-head">
        <div>
          <h1>Results for “{q}”</h1>
          <p className="muted" aria-live="polite">
            {total === 0 ? 'Nothing matched. Try fewer words, or another spelling.' : `${total.toLocaleString('en-US')} products match, the closest first${total > PAGE_SIZE ? `; ${from} to ${Math.min(total, from + cards.length - 1)}` : ''}.`}
          </p>
        </div>
      </div>
      <div className="listing">
        <FacetList s={s} facets={facets} keys={['category', 'brand', 'price', 'colour']} labels={label} />
        <div>
          <Results s={s} cards={cards} total={total} api="/api/search" />
          {total === 0 && Object.keys(filters).length > 0 && <a href={hrefOf(s, { filters: {} })}>Search again without filters</a>}
        </div>
      </div>
    </>
  );
}
