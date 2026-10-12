import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { MapBase } from '../../../components/map-base';
import { TopBar } from '../../../components/top-bar';
import { Track } from '../../../components/track';
import { asUser, one } from '../../../lib/db';
import { browserAccess, need, whoOf } from '../../../lib/session';
import type { Job } from '../../../lib/dispatch';

export const metadata: Metadata = { title: 'Your order' };
export const dynamic = 'force-dynamic';

export default async function Order({ params, searchParams }: { params: Promise<{ id: string }>; searchParams: Promise<{ late?: string }> }) {
  const a = await need('customer');
  const id = Number((await params).id);
  if (!Number.isInteger(id)) notFound();
  // Under the customer's token another customer's order is not there at
  // all: the same 404 as an order that never was.
  const order = await one<Job>(asUser(whoOf(a)), 'get orders where id = $1', [id]);
  if (!order) notFound();
  const { late } = await searchParams;
  return (
    <>
      <TopBar account={a} here="/orders" />
      <main className="page">
        <p>
          <a href="/orders">Your orders</a>
        </p>
        <Track access={browserAccess(a)} base={<MapBase />} initial={order} tooLate={!!late} />
      </main>
    </>
  );
}
