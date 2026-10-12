'use client';
// A customer following their order: a subscription to that one row under
// their own token, straight to the router. The courier's point arrives on
// the order itself (`courier_loc`), written with each beat of its lease,
// which writes the order anyway: a rule reading `orders` could let the
// customer follow the courier's row of `positions` instead, but that is a
// second stream for each customer and an inner `get` for each of them at
// every beat (README, gap 6). Who the courier is, read once as the order
// is claimed, is that rule's: their row of `couriers` (policy.txt).
import { useEffect, useState, type ReactNode } from 'react';
import { metresBetween, pathOf, svgOf, type Point } from '../lib/city';
import type { Job } from '../lib/dispatch';
import { shownState } from '../lib/states';
import { CourierMark, LiveMap, OrderPin } from './live-map';
import { since, useConnection, useNow, type Access } from './access';

const at = (iso: string | null) => (iso ? new Date(iso).toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit', timeZone: 'Etc/GMT-3' }) : '');

export function Track({ access, base, initial, tooLate }: { access: Access; base: ReactNode; initial: Job; tooLate: boolean }) {
  const conn = useConnection(access);
  const now = useNow();
  const [o, setO] = useState(initial);
  useEffect(
    () =>
      conn.subscribe('orders', { id: `eq.${initial.id}` }, (ev: { type: string; rows?: unknown[]; puts?: unknown[] }) => {
        const row = ((ev.type === 'seed' ? ev.rows : ev.puts) as Job[] | undefined)?.find((r) => r.id === initial.id);
        if (row) setO(row);
      }),
    [conn, initial.id],
  );
  // The courier carrying it, by name: read under the customer's token,
  // which reaches that one row of `couriers` while the order is theirs to
  // carry, and no other.
  const carrier = o.status === 'assigned' || o.status === 'picked' ? o.courier : null;
  const [read, setRead] = useState<{ user: string; name: string; vehicle: string } | null>(null);
  useEffect(() => {
    if (!carrier) return;
    let gone = false;
    conn
      .run('get couriers select user, name, vehicle where user = $1', [carrier])
      .then((r) => {
        const row = (r as { rows: { user: string; name: string; vehicle: string }[] }).rows?.[0];
        if (!gone && row) setRead(row);
      })
      .catch(() => {});
    return () => {
      gone = true;
    };
  }, [conn, carrier]);
  const rider = read && read.user === carrier ? read : null;
  const s = shownState(o, now);
  const courier = o.courier_loc;
  const next: Point = o.status === 'picked' ? o.dropoff : o.pickup;
  const left = courier ? metresBetween(courier, next) * 1.3 + (o.status === 'picked' ? 0 : metresBetween(o.pickup, o.dropoff) * 1.3) : null;
  const eta = left !== null ? left / 4.5 / 6 : null; // a bike's pace, at the demo's six times
  const steps = [
    { label: 'Order placed', time: at(o.created), done: true },
    { label: 'A courier took it', time: '', done: o.status !== 'ready' && o.status !== 'cancelled' && o.status !== 'dead' },
    { label: 'Picked up', time: at(o.picked_at), done: !!o.picked_at },
    { label: 'Delivered', time: at(o.delivered_at), done: o.status === 'delivered' },
  ];
  const current = steps.findIndex((x) => !x.done);
  const [px, py] = svgOf(o.pickup);
  const [dx, dy] = svgOf(o.dropoff);
  const cx = (px + dx) / 2;
  const cy = (py + dy) / 2;
  const w = Math.max(1600, Math.abs(px - dx) * 1.8, Math.abs(py - dy) * 2.6);
  return (
    <div className="two">
      <div className="track-map">
        <LiveMap base={base} initial={[cx - w / 2, cy - w * 0.36, w, w * 0.72]} label="Your order on the map">
          {(k) => (
            <>
              <path d={pathOf([o.pickup, o.dropoff])} className="leg" style={{ strokeWidth: 24 * k }} />
              <OrderPin x={px} y={py} k={k * 1.8} state="ready" title={o.pickup_name} />
              <circle cx={dx} cy={dy} r={56 * k} className="dropoff-pin" />
              {courier && o.status !== 'delivered' && (
                <CourierMark x={svgOf(courier)[0]} y={svgOf(courier)[1]} k={k * 2} heading={0} state={o.status === 'picked' ? 'dropoff' : 'pickup'} title="Your courier" />
              )}
            </>
          )}
        </LiveMap>
      </div>
      <section className="sheet" aria-live="polite">
        <h1 style={{ fontSize: 26, marginBottom: 6 }}>{o.pickup_name}</h1>
        <p className="muted" style={{ marginBottom: 14 }}>
          To {o.dropoff_addr}. Order {o.ref}.
        </p>
        {s === 'delivered' ? (
          <p className="notice good">Delivered at {at(o.delivered_at)}.</p>
        ) : s === 'cancelled' ? (
          <p className="notice bad">Cancelled.</p>
        ) : eta !== null ? (
          <p className="notice">
            {rider ? `${rider.name}, by ${rider.vehicle}: about` : 'About'} {Math.max(1, Math.round(eta / 60))} min away.
          </p>
        ) : (
          <p className="notice">Waiting {since(now - Date.parse(o.created))} for a courier.</p>
        )}
        <ol className="timeline" style={{ marginTop: 14 }}>
          {steps.map((x, i) => (
            <li key={x.label} className={x.done ? 'done' : i === current ? 'now' : ''}>
              <span>{x.label}</span>
              <time>{x.time}</time>
            </li>
          ))}
        </ol>
        {tooLate && <p className="notice bad">A courier had taken it already: it can no longer be cancelled.</p>}
        {o.status === 'ready' && (
          <form action="/api/orders" method="post" style={{ marginTop: 14 }}>
            <input type="hidden" name="cancel" value={o.id} />
            <button className="btn quiet" type="submit">
              Cancel this order
            </button>
          </form>
        )}
      </section>
    </div>
  );
}
