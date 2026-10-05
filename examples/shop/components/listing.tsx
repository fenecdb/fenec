// A listing's parts -- facets, sorts, the grid and its pages -- as links:
// every filter, order and page is an address a crawler can follow and a
// shopper can share, and all of it works with JavaScript off.
import { FILTERS, PAGE_SIZE, SORTS, bandLabel, type Facet, type FilterKey, type Filters, type Sort } from '../lib/catalog';
import { ProductCard, type CardData } from './card';
import { LoadMore } from './load-more';
import { hrefOf, type ListingState } from '../lib/listing-href';

export { hrefOf, type ListingState };

function toggled(filters: Filters, k: FilterKey, v: string): Filters {
  const now = filters[k] ?? [];
  const next = now.includes(v) ? now.filter((x) => x !== v) : [...now, v];
  const out = { ...filters };
  if (next.length) out[k] = next;
  else delete out[k];
  return out;
}

export function FacetList({ s, facets, keys, labels }: { s: ListingState; facets: Record<string, Facet[]>; keys: FilterKey[]; labels?: Map<string, string> }) {
  const active = Object.values(s.filters).some((v) => v?.length);
  return (
    <div className="filters" id="filters">
      <a className="filters-toggle" href="#filters">Filter{active ? ' (on)' : ''}</a>
      <div className="facets">
        {active && (
          <p>
            <a className="clear" href={hrefOf(s, { filters: {} })}>Clear every filter</a>
          </p>
        )}
        {keys.map((k) => {
          const counts = (facets[FILTERS[k].field] ?? []).filter((f) => f.value !== null);
          if (!counts.length) return null;
          // The price's ranges come in their order, the rest most first.
          const shown = counts;
          return (
            <section className="facet" key={k} aria-label={FILTERS[k].label}>
              <h2>{FILTERS[k].label}</h2>
              <ul>
                {shown.slice(0, 12).map((f) => {
                  const on = (s.filters[k] ?? []).includes(f.value!);
                  return (
                    <li key={f.value}>
                      <a href={hrefOf(s, { filters: toggled(s.filters, k, f.value!) })} aria-current={on ? 'true' : undefined} rel="nofollow">
                        <span>{k === 'price' ? bandLabel(f.value) : (labels?.get(f.value!) ?? f.value)}</span>
                        <span className="count">{f.count}</span>
                      </a>
                    </li>
                  );
                })}
              </ul>
            </section>
          );
        })}
        <a className="button quiet filters-close" href="#results">Done</a>
      </div>
    </div>
  );
}


export function SortLinks({ s }: { s: ListingState }) {
  return (
    <nav aria-label="Sort">
      <ul className="sorts">
        {(Object.keys(SORTS) as Sort[]).map((k) => (
          <li key={k}>
            <a href={hrefOf(s, { sort: k })} aria-current={s.sort === k ? 'true' : undefined} rel="nofollow">
              {SORTS[k].label}
            </a>
          </li>
        ))}
      </ul>
    </nav>
  );
}

export function Results({ s, cards, total, api }: { s: ListingState; cards: CardData[]; total: number; api: string }) {
  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const near = [1, s.page - 1, s.page, s.page + 1, pages].filter((p, i, a) => p >= 1 && p <= pages && a.indexOf(p) === i).sort((a, b) => a - b);
  const next = s.page < pages ? hrefOf(s, { page: s.page + 1 }) : null;
  return (
    <div>
      <ul className="grid" id="results">
        {cards.map((p, i) => (
          <ProductCard key={p.sku} p={p} eager={i < 4} />
        ))}
      </ul>
      {next && <LoadMore api={`${api}${hrefOf(s, { page: s.page + 1 }).slice(s.base.length)}`} next={next} />}
      {pages > 1 && (
        <nav className="pages" aria-label="Pages">
          {s.page > 1 && <a href={hrefOf(s, { page: s.page - 1 })} rel="prev">Previous</a>}
          {near.map((p, i) => (
            <span key={p} style={{ display: 'contents' }}>
              {i > 0 && near[i - 1] !== p - 1 && <span aria-hidden="true">…</span>}
              {p === s.page ? <span aria-current="page">{p}</span> : <a href={hrefOf(s, { page: p })}>{p}</a>}
            </span>
          ))}
          {next && <a href={next} rel="next">Next</a>}
        </nav>
      )}
    </div>
  );
}
