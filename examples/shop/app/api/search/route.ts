import type { NextRequest } from 'next/server';
import { listingJson } from '../../../lib/listing-api';

export async function GET(req: NextRequest) {
  return listingJson(req, '/search', '/api/search');
}
