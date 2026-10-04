import type { NextRequest } from 'next/server';
import { category } from '../../../../lib/catalog';
import { listingJson } from '../../../../lib/listing-api';
import { json } from '../../../../lib/http';

export async function GET(req: NextRequest, { params }: { params: Promise<{ slug: string }> }) {
  const { slug } = await params;
  if (!(await category(slug))) return json({ error: 'no such category' }, 404);
  return listingJson(req, `/c/${slug}`, `/api/c/${slug}`, slug);
}
