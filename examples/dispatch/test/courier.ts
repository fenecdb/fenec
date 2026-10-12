// One courier as a process of its own, for the lease test to kill: it
// claims the nearest order to a point, picks it up, and beats every
// `beat` ms -- position and lease -- writing a JSON line for each step,
// until it is killed.
import { routeTo } from '../lib/db';
import { claimNearest, heartbeat, pickUp, queue } from '../lib/dispatch';
import type { Point } from '../lib/city';

const [router, sub, company, lon, lat, lease, beat] = process.argv.slice(2);
routeTo(router);
queue.leaseMs = Number(lease);
const who = { sub, role: 'courier' as const, company };
const at: Point = [Number(lon), Number(lat)];
const say = (o: object) => process.stdout.write(`${JSON.stringify({ ...o, t: Date.now() })}\n`);
const got = await claimNearest(who, at, { radius: 2_000, wait: 10_000 });
if (!got) {
  say({ event: 'none' });
  process.exit(1);
}
say({ event: 'claimed', id: got.job.id, claim: got.job.claim });
await pickUp(who, got.job);
say({ event: 'picked' });
for (;;) {
  const r = await heartbeat(who, { at, state: 'dropoff', job: got.job });
  say({ event: 'beat', kept: r.kept });
  await new Promise((r) => setTimeout(r, Number(beat)));
}
