// A product's picture as a file, for JSON-LD's `image` and Open Graph. The
// pages draw the same picture inline and never ask for this.
import { db } from '../../../../lib/db';
import { products } from '../../../../lib/schema';
import { svgFile } from '../../../../lib/shapes';
import { CATEGORY_SHAPES } from '../../../../lib/category-shapes';

export async function GET(_req: Request, { params }: { params: Promise<{ file: string }> }) {
  const { file } = await params;
  const m = /^(SG-\d{6})\.svg$/.exec(file);
  if (!m) return new Response('not found', { status: 404 });
  const shop = await db();
  const p = await shop.from(products).select('sku', 'category', 'colour').where('sku', m[1]).first();
  if (!p) return new Response('not found', { status: 404 });
  return new Response(svgFile(CATEGORY_SHAPES[p.category] ?? 'cube', p.colour, p.sku), {
    headers: { 'content-type': 'image/svg+xml', 'cache-control': 'public, max-age=86400, immutable' },
  });
}
