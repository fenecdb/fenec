import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { category, listing, readParams, PAGE_SIZE, SORTS } from '../../../lib/catalog';
import { FacetList, Results, SortLinks, hrefOf, type ListingState } from '../../../components/listing';
import { Breadcrumbs, JsonLd, SITE } from '../../../components/seo';

type Props = { params: Promise<{ slug: string }>; searchParams: Promise<Record<string, string | string[] | undefined>> };

export async function generateMetadata({ params, searchParams }: Props): Promise<Metadata> {
  const { slug } = await params;
  const c = await category(slug);
  if (!c) return {};
  const { filters, sort, page } = readParams(await searchParams);
  const filtered = Object.keys(filters).length > 0 || sort !== 'featured';
  const title = page > 1 ? `${c.name}, page ${page}` : c.name;
  return {
    title,
    description: `${c.blurb} Shop ${c.name.toLowerCase()} from desert-tested makers at Sandgrouse.`,
    // The canonical page of a listing is its page alone: a filter or an
    // order is a view of it, kept out of the index but followed.
    alternates: { canonical: page > 1 ? `/c/${slug}?page=${page}` : `/c/${slug}` },
    robots: filtered ? { index: false, follow: true } : undefined,
    openGraph: { title: `${title} | Sandgrouse`, url: `/c/${slug}`, type: 'website' },
  };
}

export default async function CategoryPage({ params, searchParams }: Props) {
  const { slug } = await params;
  const c = await category(slug);
  if (!c) notFound();
  const { filters, sort, page } = readParams(await searchParams);
  const { cards, facets, total } = await listing({ category: slug, filters, sort, page });
  if (page > 1 && cards.length === 0) notFound();
  const s: ListingState = { base: `/c/${slug}`, filters, sort, page };
  const from = (page - 1) * PAGE_SIZE + 1;
  return (
    <>
      <Breadcrumbs trail={[{ name: 'Home', href: '/' }, { name: c.department, href: `/#${c.department.toLowerCase().replace(/[^a-z]+/g, '-')}` }, { name: c.name, href: `/c/${slug}` }]} />
      <JsonLd
        data={{
          '@context': 'https://schema.org',
          '@type': 'ItemList',
          name: c.name,
          numberOfItems: total,
          itemListElement: cards.map((p, i) => ({ '@type': 'ListItem', position: from + i, url: `${SITE}/p/${p.slug}`, name: p.name })),
        }}
      />
      <div className="listing-head">
        <div>
          <h1>{c.name}</h1>
          <p className="muted">{c.blurb}</p>
        </div>
        <SortLinks s={s} />
      </div>
      <div className="listing">
        <FacetList s={s} facets={facets} keys={['brand', 'price', 'colour', 'material']} />
        <div>
          <p className="muted" aria-live="polite">
            {total === 0
              ? 'Nothing matches every filter; take one off.'
              : `${total.toLocaleString('en-US')} products${total > PAGE_SIZE ? `, showing ${from} to ${Math.min(total, from + cards.length - 1)}` : ''}, ${SORTS[sort].label.toLowerCase()}.`}
          </p>
          <Results s={s} cards={cards} total={total} api={`/api/c/${slug}`} />
          {total === 0 && <a href={hrefOf(s, { filters: {} })}>Show every {c.name.toLowerCase()}</a>}
        </div>
      </div>
    </>
  );
}
