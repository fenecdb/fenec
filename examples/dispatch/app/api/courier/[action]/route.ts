// A courier's phone talks to these: claim, beat, pick up, deliver, give
// back. Each runs under the courier's own token (lib/dispatch.ts); the
// claim's first step reads the nearest ready orders with the app's, since a
// courier's token reads only orders already theirs.
import type { NextRequest } from 'next/server';
import { crossSite, json, readBody } from '../../../../lib/http';
import { current, whoOf } from '../../../../lib/session';
import { claimNearest, deliver, giveBack, heartbeat, pickUp } from '../../../../lib/dispatch';
import { CLAIM_WAIT_MS } from '../../../../lib/config';
import { FenecError } from '../../../../lib/db';
import type { Point } from '../../../../lib/city';

const point = (v: unknown): Point | null =>
  Array.isArray(v) && v.length === 2 && v.every((n) => typeof n === 'number' && Number.isFinite(n)) ? [v[0], v[1]] : null;

const job = (v: unknown): { id: number; claim: string } | null => {
  const o = v as { id?: unknown; claim?: unknown } | null;
  return o && Number.isInteger(o.id) && typeof o.claim === 'string' ? { id: o.id as number, claim: o.claim } : null;
};

export async function POST(req: NextRequest, ctx: { params: Promise<{ action: string }> }) {
  if (crossSite(req)) return json({ error: 'from Plover itself' }, 403);
  const a = await current();
  if (!a || a.role !== 'courier') return json({ error: 'sign in as a courier' }, 401);
  const who = whoOf(a);
  const { body } = await readBody(req);
  const { action } = await ctx.params;
  try {
    switch (action) {
      case 'claim': {
        const at = point(body.at);
        if (!at) return json({ error: 'where are you: at is [lon, lat]' }, 400);
        // Held at the server while nothing near is ready; the phone's
        // request is held with it, and given up if the phone goes.
        const got = await claimNearest(who, at, { wait: CLAIM_WAIT_MS, signal: req.signal });
        return json({ job: got?.job ?? null, how: got?.how ?? null });
      }
      case 'beat': {
        const at = point(body.at);
        if (!at) return json({ error: 'at is [lon, lat]' }, 400);
        const r = await heartbeat(who, {
          at,
          state: ['idle', 'pickup', 'dropoff'].includes(String(body.state)) ? String(body.state) : 'idle',
          heading: Number(body.heading) || 0,
          vehicle: typeof body.vehicle === 'string' ? body.vehicle : undefined,
          job: job(body.job),
        });
        return json(r);
      }
      case 'pickup': {
        const j = job(body);
        return j ? json({ ok: await pickUp(who, j) }) : json({ error: 'which order' }, 400);
      }
      case 'deliver': {
        const j = job(body);
        return j ? json({ ok: await deliver(who, j, Number(body.driven) || 0) }) : json({ error: 'which order' }, 400);
      }
      case 'giveback': {
        const j = job(body);
        return j ? json({ ok: await giveBack(who, j, String(body.why ?? 'given back by the courier').slice(0, 200)) }) : json({ error: 'which order' }, 400);
      }
      default:
        return json({ error: `no action ${action}` }, 404);
    }
  } catch (e) {
    if (req.signal.aborted) return json({ error: 'gone' }, 499);
    if (e instanceof FenecError && e.status) return json({ error: e.message, quota: e.quota ?? null, retryAfter: e.retryAfter ?? null }, e.status);
    throw e;
  }
}
