// What the shop's route handlers share: a body read from a form or from
// JSON, an answer as a redirect for a form or JSON for a script, and a
// refusal of a write sent from another site.
import 'server-only';
import { NextResponse, type NextRequest } from 'next/server';

export async function readBody(req: NextRequest): Promise<{ body: Record<string, unknown>; form: boolean }> {
  const type = req.headers.get('content-type') ?? '';
  if (type.includes('application/json')) {
    const body = await req.json().catch(() => null);
    return { body: body && typeof body === 'object' && !Array.isArray(body) ? body : {}, form: false };
  }
  const data = await req.formData().catch(() => null);
  return { body: data ? Object.fromEntries([...data.entries()].map(([k, v]) => [k, typeof v === 'string' ? v : ''])) : {}, form: true };
}

/**
 * A write must come from the shop's own pages. Cookies are `SameSite=Lax`,
 * so another site's form posts without them; this refuses it outright
 * whenever the browser says where it came from.
 */
export function crossSite(req: NextRequest): boolean {
  const origin = req.headers.get('origin');
  if (!origin) return false;
  const host = req.headers.get('x-forwarded-host') ?? req.headers.get('host');
  try {
    return new URL(origin).host !== host;
  } catch {
    return true;
  }
}

/** After a form's write, back to a page (303: the browser asks for it with GET). */
export function back(req: NextRequest, path: string): NextResponse {
  return NextResponse.redirect(new URL(path, req.nextUrl.origin), 303);
}

export function json(body: unknown, status = 200): NextResponse {
  return NextResponse.json(body, { status, headers: { 'cache-control': 'no-store' } });
}
