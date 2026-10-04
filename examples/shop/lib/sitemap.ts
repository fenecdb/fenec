// The sitemaps: an index, one file of the pages that are not products, and
// the products 5 000 to a file -- under the 50 000 a sitemap may hold, and
// small enough to fetch quickly.
import 'server-only';
import { allSlugs, categoryList } from './catalog';
import { SITE } from '../components/seo';

export const PER_FILE = 5000;

const esc = (s: string) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;');

export function xml(body: string): Response {
  return new Response(`<?xml version="1.0" encoding="UTF-8"?>\n${body}`, {
    headers: { 'content-type': 'application/xml; charset=utf-8', 'cache-control': 'public, max-age=3600' },
  });
}

export async function productFiles(): Promise<number> {
  return Math.max(1, Math.ceil((await allSlugs()).length / PER_FILE));
}

export async function index(): Promise<string> {
  const n = await productFiles();
  const files = ['pages', ...Array.from({ length: n }, (_, i) => `products-${i + 1}`)];
  return `<sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">${files
    .map((f) => `<sitemap><loc>${SITE}/sitemaps/${f}.xml</loc></sitemap>`)
    .join('')}</sitemapindex>`;
}

export async function file(name: string): Promise<string | null> {
  const urls: { loc: string; lastmod?: string }[] = [];
  if (name === 'pages') {
    urls.push({ loc: `${SITE}/` });
    for (const c of await categoryList()) urls.push({ loc: `${SITE}/c/${c.slug}` });
  } else {
    const m = /^products-(\d+)$/.exec(name);
    if (!m) return null;
    const i = Number(m[1]) - 1;
    const part = (await allSlugs()).slice(i * PER_FILE, (i + 1) * PER_FILE);
    if (!part.length) return null;
    for (const p of part) urls.push({ loc: `${SITE}/p/${p.slug}`, lastmod: p.added?.slice(0, 10) });
  }
  return `<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">${urls
    .map((u) => `<url><loc>${esc(u.loc)}</loc>${u.lastmod ? `<lastmod>${u.lastmod}</lastmod>` : ''}</url>`)
    .join('')}</urlset>`;
}
