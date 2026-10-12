import type { Metadata } from 'next';
import { MapBase } from '../../components/map-base';
import { TopBar } from '../../components/top-bar';
import { Console, type Ord, type Pos, type Roster } from '../../components/console';
import { asUser, rows } from '../../lib/db';
import { browserAccess, need, whoOf } from '../../lib/session';

export const metadata: Metadata = { title: 'Live map' };
export const dynamic = 'force-dynamic';

export default async function Dispatch() {
  const a = await need('dispatcher');
  // Read with the dispatcher's own token, as the page's streams will be.
  const db = asUser(whoOf(a));
  const [orders, positions, roster] = await Promise.all([
    rows<Ord>(db, 'get orders where status in ["ready", "assigned", "picked", "dead"] limit 2000'),
    rows<Pos>(db, 'get positions'),
    rows<Roster>(db, 'get couriers select user, code, name, vehicle'),
  ]);
  return (
    <div className="console">
      <TopBar account={a} here="/dispatch" />
      <Console access={browserAccess(a)} base={<MapBase />} orders={orders} positions={positions} roster={roster} />
    </div>
  );
}
