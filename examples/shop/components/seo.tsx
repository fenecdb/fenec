// Structured data and breadcrumbs, written into the HTML the server sends:
// a crawler that runs no JavaScript reads all of it.

export const SITE = (process.env.SITE_URL ?? 'http://localhost:3000').replace(/\/$/, '');

/** A JSON-LD block; `<` is escaped so no product copy can close the script. */
export function JsonLd({ data }: { data: unknown }) {
  const json = JSON.stringify(data).replace(/</g, '\\u003c');
  return <script type="application/ld+json" dangerouslySetInnerHTML={{ __html: json }} />;
}

export function Breadcrumbs({ trail }: { trail: { name: string; href: string }[] }) {
  return (
    <>
      <nav aria-label="Breadcrumb" className="crumbs">
        <ol>
          {trail.map((t, i) => (
            <li key={t.href}>{i < trail.length - 1 ? <a href={t.href}>{t.name}</a> : <span aria-current="page">{t.name}</span>}</li>
          ))}
        </ol>
      </nav>
      <JsonLd
        data={{
          '@context': 'https://schema.org',
          '@type': 'BreadcrumbList',
          itemListElement: trail.map((t, i) => ({ '@type': 'ListItem', position: i + 1, name: t.name, item: `${SITE}${t.href}` })),
        }}
      />
    </>
  );
}
