import type { Metadata } from 'next';
import { bestRated, categoryList, newest } from '../lib/catalog';
import { ProductArt } from '../components/art';
import { ProductCard } from '../components/card';
import { JsonLd, SITE } from '../components/seo';
import { CATEGORY_SHAPES } from '../lib/category-shapes';
import { DEPARTMENTS, departmentId } from '../lib/departments';
import { money } from '../lib/money';

// Made once and kept, made again in the background every five minutes
// (incremental static regeneration): the home page is the same for everyone.
export const revalidate = 300;

export const metadata: Metadata = {
  alternates: { canonical: '/' },
};

export default async function Home() {
  const [cats, top, fresh] = await Promise.all([categoryList(), bestRated(12), newest(8)]);
  // Three of the best rated, each of another kind, for the plates up top.
  const plates = top.filter((p, i, all) => all.findIndex((q) => q.category === p.category) === i).slice(0, 3);
  return (
    <>
      <JsonLd
        data={{
          '@context': 'https://schema.org',
          '@type': 'WebSite',
          name: 'Sandgrouse',
          url: SITE,
          potentialAction: { '@type': 'SearchAction', target: `${SITE}/search?q={query}`, 'query-input': 'required name=query' },
        }}
      />
      <section className="hero">
        <div>
          <h1>Carry water, shade and light across dry country.</h1>
          <p>
            The sandgrouse flies forty kilometres to water and carries it home in its feathers. We stock the gear that does the same job
            for people: shelters that hold in a crosswind, bottles that stay cold all afternoon, and clothes that keep the sun off.
          </p>
          <a className="button" href="/c/tents">Shop tents</a>
        </div>
        <ul className="plates" aria-label="Best rated this season">
          {plates.map((p, i) => (
            <li key={p.sku}>
              <a href={`/p/${p.slug}`}>
                <ProductArt shape={CATEGORY_SHAPES[p.category] ?? 'cube'} colour={p.colour} sku={p.sku} className="plate" label={i === 0 ? p.name : undefined} />
                <span className="card-name">{p.name}</span>
              </a>
              <span className="price">{money(p.price)}</span>
            </li>
          ))}
        </ul>
      </section>

      <section className="depts-list" aria-label="Shop by category">
        {DEPARTMENTS.map((d) => {
          const mine = cats.filter((c) => c.department === d);
          if (!mine.length) return null;
          return (
            <div key={d} id={departmentId(d)}>
              <h2>{d}</h2>
              <ul>
                {mine.map((c) => (
                  <li key={c.slug}>
                    <a href={`/c/${c.slug}`}>{c.name}</a>
                  </li>
                ))}
              </ul>
            </div>
          );
        })}
      </section>

      <section className="section">
        <div className="section-head">
          <h2>Best rated by people who used them</h2>
        </div>
        <ul className="grid">
          {top.slice(0, 8).map((p) => (
            <ProductCard key={p.sku} p={p} />
          ))}
        </ul>
      </section>
      <section className="section">
        <div className="section-head">
          <h2>New this season</h2>
        </div>
        <ul className="grid">
          {fresh.map((p) => (
            <ProductCard key={p.sku} p={p} />
          ))}
        </ul>
      </section>
    </>
  );
}
