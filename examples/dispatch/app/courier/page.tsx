import type { Metadata } from 'next';
import { MapBase } from '../../components/map-base';
import { TopBar } from '../../components/top-bar';
import { CourierApp, type Delivery } from '../../components/courier-app';
import { asUser, one, rows } from '../../lib/db';
import { need, whoOf } from '../../lib/session';
import { people, COMPANIES } from '../../lib/world';
import type { Job } from '../../lib/dispatch';
import type { Point } from '../../lib/city';
import { merrowMidnight } from '../../lib/insights';

export const metadata: Metadata = { title: 'My shift' };
export const dynamic = 'force-dynamic';

export default async function Courier() {
  const a = await need('courier');
  const db = asUser(whoOf(a));
  const me = people(COMPANIES.find((c) => c.slug === a.company)!).find((p) => p.uid === a.uid);
  const since = merrowMidnight();
  const [row, pos, carrying, recent, today] = await Promise.all([
    one<{ vehicle: string; code: string }>(db, 'get couriers select vehicle, code where user = $1', [a.uid]),
    one<{ loc: Point }>(db, 'get positions select loc where courier = $1', [a.uid]),
    // An order this courier holds, its lease running: a page opened again picks it up.
    one<Job>(db, 'get orders where courier = $1 and status in ["assigned", "picked"] and run_at > now() limit 1', [a.uid]),
    // The latest ten, a run of @sorted(courier, delivered_at desc) read to the page.
    rows<Delivery>(db, 'get orders select id, pickup_name, dropoff_addr, fee, delivered_at, driven where courier = $1 and delivered_at is not null order delivered_at desc limit 10', [a.uid]),
    one<{ n: number; fees: number | null; metres: number | null }>(
      db,
      'get orders select count(*) as n, sum(fee) as fees, sum(driven) as metres where courier = $1 and delivered_at >= $2',
      [a.uid, since],
    ),
  ]);
  return (
    <div className="phone">
      <TopBar account={a} here="/courier" />
      <CourierApp
        base={<MapBase />}
        name={a.name}
        vehicle={row?.vehicle ?? me?.vehicle ?? 'bike'}
        start={pos?.loc ?? me?.home.point ?? [26.99, 38.35]}
        carrying={carrying ?? null}
        recent={recent}
        today={{ n: today?.n ?? 0, fees: today?.fees ?? 0, metres: today?.metres ?? 0 }}
      />
    </div>
  );
}
