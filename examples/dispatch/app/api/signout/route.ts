import type { NextRequest } from 'next/server';
import { back } from '../../../lib/http';
import { endSession } from '../../../lib/session';

export async function POST(req: NextRequest) {
  await endSession();
  return back(req, '/signin');
}
