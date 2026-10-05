// Sign up, sign in and out. A password is kept as scrypt, a salt and a
// hash; the email is `@unique`, so two accounts cannot share one even when
// two sign-ups race (`insert` refuses the second with 409).
import type { NextRequest } from 'next/server';
import { db } from '../../../lib/db';
import { users } from '../../../lib/schema';
import { checkPassword, hashPassword, signIn, signOut } from '../../../lib/session';
import { back, crossSite, json, readBody } from '../../../lib/http';

const str = (v: unknown, max: number) => (typeof v === 'string' ? v.trim().slice(0, max) : '');

export async function POST(req: NextRequest) {
  if (crossSite(req)) return json({ error: 'cross-site request refused' }, 403);
  const { body, form } = await readBody(req);
  const done = (ok: boolean, code: string, status = 400) =>
    form ? back(req, ok ? '/account' : `/account?error=${code}`) : json(ok ? { ok } : { error: code }, ok ? 200 : status);
  const shop = await db();

  if (body.action === 'signout') {
    await signOut();
    return done(true, '');
  }
  const email = str(body.email, 200).toLowerCase();
  const password = str(body.password, 200);
  if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email) || password.length < 8) return done(false, 'invalid');

  if (body.action === 'signup') {
    const name = str(body.name, 80) || email.split('@')[0];
    try {
      await shop.from(users).insert({ email, name, password: hashPassword(password), created: new Date() });
    } catch {
      return done(false, 'taken', 409);
    }
  }
  const u = await shop.from(users).where('email', email).first();
  if (!u || !checkPassword(password, u.password)) return done(false, 'wrong', 401);
  await signIn({ id: u.id, name: u.name, email: u.email });
  return done(true, '');
}
