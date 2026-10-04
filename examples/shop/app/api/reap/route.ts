// Releases lapsed reservations. Every checkout reaps first; a cron job
// calls this with the shop's admin token so stock comes back even when
// nobody checks out.
import { timingSafeEqual } from 'node:crypto';
import type { NextRequest } from 'next/server';
import { reap } from '../../../lib/checkout';
import { json } from '../../../lib/http';

const ADMIN = Buffer.from(`Bearer ${process.env.SHOP_ADMIN_TOKEN ?? 'shop-dev-admin'}`);

export async function POST(req: NextRequest) {
  const got = Buffer.from(req.headers.get('authorization') ?? '');
  if (got.length !== ADMIN.length || !timingSafeEqual(got, ADMIN)) return json({ error: 'unauthorized' }, 401);
  return json({ released: await reap() });
}
