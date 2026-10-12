import type { NextRequest } from 'next/server';
import { back, crossSite, json, readBody } from '../../../lib/http';
import { home, signIn, startSession } from '../../../lib/session';

export async function POST(req: NextRequest) {
  if (crossSite(req)) return json({ error: 'a sign-in comes from Plover itself' }, 403);
  const { body, form } = await readBody(req);
  const a = await signIn(String(body.email ?? ''), String(body.password ?? ''));
  if (!a) return form ? back(req, '/signin?failed=1') : json({ error: 'that email and password do not match an account' }, 401);
  await startSession(a);
  return form ? back(req, home(a)) : json({ uid: a.uid, role: a.role, company: a.company });
}
