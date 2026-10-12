import type { Metadata, Viewport } from 'next';
import localFont from 'next/font/local';
import './globals.css';

// Overpass, the open descendant of the US highway signs' Highway Gothic:
// a city's road signage, for a product about its roads. One variable file,
// the Latin subset, 400 to 800 (39 KB).
const overpass = localFont({
  src: './fonts/overpass-latin-var.woff2',
  weight: '400 800',
  display: 'swap',
  variable: '--overpass',
  adjustFontFallback: 'Arial',
});

export const metadata: Metadata = {
  title: { default: 'Plover: courier dispatch', template: '%s | Plover' },
  description: 'Dispatch for delivery companies: orders to the nearest courier, every courier on the map, every delivery accounted for.',
};

export const viewport: Viewport = { themeColor: '#22303b', width: 'device-width', initialScale: 1 };

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" className={overpass.variable}>
      <body>{children}</body>
    </html>
  );
}
