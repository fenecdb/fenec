import type { Metadata, Viewport } from 'next';
import localFont from 'next/font/local';
import { SITE } from '../components/seo';
import { DEPARTMENTS, departmentId } from '../lib/departments';
import './globals.css';

// Schibsted Grotesk at 800 for what is display type -- the wordmark,
// headings, prices, buttons -- and the system's sans for running text. The
// display face is self-hosted and cut to ASCII and a few marks (15.7 KB):
// the variable font, every weight, was 47 KB and cost the product page
// 265 ms of LCP on Lighthouse's slow 4G (README, "Page speed").
const display = localFont({
  src: './fonts/schibsted-grotesk-800-subset.woff2',
  weight: '800',
  display: 'swap',
  variable: '--display',
  adjustFontFallback: 'Arial',
});

export const metadata: Metadata = {
  metadataBase: new URL(SITE),
  title: { default: 'Sandgrouse: desert and travel gear', template: '%s | Sandgrouse' },
  description: 'Shelter, water, light and clothing for dry country, from thirty-two makers. Free shipping over $150.',
  openGraph: { siteName: 'Sandgrouse', type: 'website', locale: 'en_US' },
};

export const viewport: Viewport = { themeColor: '#1e2758', width: 'device-width', initialScale: 1 };


export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" className={display.variable}>
      <body>
        <a className="skip" href="#main">Skip to the content</a>
        <header className="top">
          <div className="top-row wrap">
            <a href="/" className="wordmark" aria-label="Sandgrouse, home">
              Sandgrouse
            </a>
            <form action="/search" method="get" role="search" className="find">
              <label htmlFor="q" className="sr">Search the shop</label>
              <input id="q" name="q" type="search" placeholder="Search tents, stoves, shemaghs…" autoComplete="off" />
              <button type="submit">Search</button>
            </form>
            <nav aria-label="Your account" className="you">
              <a href="/account">Account</a>
              <a href="/cart">Cart</a>
            </nav>
          </div>
          <nav aria-label="Departments" className="depts wrap">
            {DEPARTMENTS.map((d) => (
              <a key={d} href={`/#${departmentId(d)}`}>{d}</a>
            ))}
          </nav>
        </header>
        <main id="main" className="wrap">{children}</main>
        <footer className="foot">
          <div className="wrap foot-row">
            <p>
              <strong>Sandgrouse</strong> carries gear for dry country: shade, water, light and the clothes that keep the sun off.
              Free shipping on orders over $150; returns within 60 days.
            </p>
            <p className="muted">
              A demonstration shop: every product, maker and review here is generated, and no card is charged. It runs on{' '}
              <a href="https://github.com/fenecdb/fenec/tree/main/examples/shop">fenecdb</a>.
            </p>
          </div>
        </footer>
      </body>
    </html>
  );
}
