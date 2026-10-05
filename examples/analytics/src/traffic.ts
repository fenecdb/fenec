// A trickle of invented visits while the demo runs, sent as a browser's
// tracker would send them -- beacons to POST /e, with the visitor's user
// agent and language -- so the dashboard's "now" moves and every event
// takes the whole path: ingest, the raw events, the change stream, the
// rollups.
import { DEMO_SITES } from './setup.ts';
import { rng, Traffic, userAgent, type SimEvent } from './sim.ts';
import { dayOf } from './time.ts';

const LANG: Record<string, string> = { US: 'en-US', DE: 'de-DE', GB: 'en-GB', IN: 'en-IN', TR: 'tr-TR', FR: 'fr-FR', BR: 'pt-BR', CA: 'en-CA', NL: 'nl-NL', JP: 'ja-JP', ES: 'es-ES', PL: 'pl-PL', AU: 'en-AU', SE: 'sv-SE', MX: 'es-MX' };

export async function sendBeacon(base: string, site: { key: string; origin: string }, batch: string, events: SimEvent[], now = Date.now()): Promise<number> {
  const e = events[0];
  const res = await fetch(`${base}/e`, {
    method: 'POST',
    headers: {
      'content-type': 'text/plain;charset=UTF-8',
      origin: site.origin,
      'user-agent': userAgent(e.device, e.browser),
      'accept-language': `${LANG[e.country] ?? 'en-US'},en;q=0.8`,
    },
    body: JSON.stringify({
      k: site.key,
      b: batch,
      u: e.user.padEnd(8, '0'),
      t: now,
      e: events.map((x) => ({ n: x.name, p: x.path, r: x.ref ? `https://${x.ref}/` : '', t: now - Math.max(0, Math.min(3_000_000, now - x.at)), d: x.props ?? undefined })),
    }),
  });
  await res.body?.cancel();
  return res.status;
}

export function startDemoTraffic(base: string): () => void {
  const r = rng(Date.now() & 0xffffff);
  const timers = new Set<ReturnType<typeof setTimeout>>();
  const plans = [
    { site: DEMO_SITES[0], every: 12000, traffic: new Traffic({ daily: 1, growth: 0, seed: 101, prefix: 'fl' }) },
    { site: DEMO_SITES[1], every: 45000, traffic: new Traffic({ daily: 1, growth: 0, seed: 202, prefix: 'tl' }) },
  ];
  let stopped = false;
  for (const p of plans) {
    const visit = () => {
      if (stopped) return;
      const now = Date.now();
      // A session as the simulator makes one, its pages sent as they happen.
      // About the seeded history's pace: a few events a minute.
      const events = p.traffic.day(dayOf(now), 0).slice(0, 5);
      const start = events[0]?.at ?? now;
      let batch = 0;
      const id = `${now.toString(36)}${Math.floor(r() * 1e9).toString(36)}`.padEnd(12, 'x');
      for (const e of events) {
        const t = setTimeout(
          () => {
            timers.delete(t);
            if (stopped) return;
            void sendBeacon(base, { key: p.site.key, origin: p.site.origins[0] }, `${id}${batch++}`, [{ ...e, at: Date.now() }]).catch(() => {});
          },
          Math.min(120_000, (e.at - start) / 4),
        );
        timers.add(t);
      }
      const t = setTimeout(visit, p.every * (0.3 + r() * 1.4));
      timers.add(t);
    };
    visit();
  }
  return () => {
    stopped = true;
    for (const t of timers) clearTimeout(t);
  };
}
