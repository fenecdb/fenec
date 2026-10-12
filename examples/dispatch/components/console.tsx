'use client';
// The dispatcher's live map. Two subscriptions under the dispatcher's own
// token, straight to the router: every courier's position, and every open
// order. Pick an order and two live queries follow it -- its ten nearest
// idle couriers (`near loc`) and who is within a radius of its pickup
// (`distance(loc, $1) <= $2`), each run again as positions are written.
import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import type { FenecHttp } from '@fenecdb/web/client';
import { svgOf, type Point } from '../lib/city';
import { CourierMark, LiveMap, OrderPin } from './live-map';
import { since, useConnection, useNow, type Access } from './access';
import { shownState } from '../lib/states';

export interface Pos {
  id: number;
  courier: string;
  loc: Point;
  seen: string;
  state: string;
  heading: number | null;
  job: number | null;
  vehicle: string | null;
}

export interface Ord {
  id: number;
  ref: string;
  merchant: string;
  pickup: Point;
  pickup_name: string;
  dropoff: Point;
  dropoff_addr: string;
  status: string;
  run_at: string | null;
  courier: string | null;
  attempts: number;
  created: string;
  promised: string;
  picked_at: string | null;
  fee: number;
  items: number;
  error: string | null;
}

export interface Roster {
  user: string;
  code: string;
  name: string;
  vehicle: string;
}

interface Near {
  courier: string;
  state: string;
  vehicle: string | null;
  loc: Point;
  m: number;
}

const PACE: Record<string, number> = { bike: 4.2, scooter: 6.5, car: 7.5 };
const OPEN = 'status in ["ready", "assigned", "picked", "dead"]';
const NEAREST = 'get positions select courier, state, vehicle, loc, distance(loc, $1) as m where state = "idle" near loc $1 limit 10';
const WITHIN = 'get positions select state, count(*) as n where distance(loc, $1) <= $2 group state';


const STATE_WORD: Record<string, string> = {
  ready: 'Waiting',
  assigned: 'To pickup',
  picked: 'Carrying',
  lapsed: 'Lease lapsed',
  dead: 'Dead letter',
  idle: 'Idle',
  pickup: 'To pickup',
  dropoff: 'Carrying',
};

function useRows<T extends { id: number }>(conn: FenecHttp, collection: string, shape: Record<string, string>, initial: T[]) {
  const [rows, setRows] = useState(() => new Map(initial.map((r) => [r.id, r])));
  const [live, setLive] = useState(false);
  const key = JSON.stringify(shape);
  useEffect(() => {
    // Changes gathered and drawn once a frame: forty couriers beating every
    // few seconds is a change every few milliseconds.
    let pending: ((m: Map<number, T>) => void)[] = [];
    let frame = 0;
    const flush = () => {
      frame = 0;
      const todo = pending;
      pending = [];
      setRows((m) => {
        const next = new Map(m);
        for (const f of todo) f(next);
        return next;
      });
    };
    const stop = conn.subscribe(
      collection,
      JSON.parse(key),
      (ev: { type: string; rows?: unknown[]; puts?: unknown[]; dels?: unknown[] }) => {
        if (ev.type === 'seed') {
          const seeded = new Map((ev.rows as T[]).map((r) => [r.id, r]));
          pending = [];
          setRows(seeded);
          return;
        }
        pending.push((m) => {
          for (const r of (ev.puts ?? []) as T[]) m.set(r.id, r);
          for (const d of (ev.dels ?? []) as (number | { id: number })[]) m.delete(typeof d === 'number' ? d : d.id);
        });
        if (!frame) frame = requestAnimationFrame(flush);
      },
      { onState: (s: string) => setLive(s === 'open'), onError: () => setLive(false) },
    );
    return () => {
      stop();
      if (frame) cancelAnimationFrame(frame);
    };
  }, [conn, collection, key]);
  return { rows, live };
}

function useLive<T>(conn: FenecHttp, text: string | null, params: unknown[], collections: string[]): T[] {
  const [rows, setRows] = useState<T[]>([]);
  const key = JSON.stringify(params);
  useEffect(() => {
    if (!text) return;
    const stop = conn.live([text, JSON.parse(key)], (r: unknown[]) => setRows(r as T[]), { collections, onError: () => {} });
    return () => {
      stop();
      setRows([]);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [conn, text, key]);
  return rows;
}

export function Console({
  access,
  base,
  orders,
  positions,
  roster,
}: {
  access: Access;
  base: ReactNode;
  orders: Ord[];
  positions: Pos[];
  roster: Roster[];
}) {
  const conn = useConnection(access);
  const now = useNow();
  const pos = useRows<Pos>(conn, 'positions', {}, positions);
  const ord = useRows<Ord>(conn, 'orders', { where: OPEN }, orders);
  const people = useMemo(() => new Map(roster.map((r) => [r.user, r])), [roster]);
  const [picked, setPicked] = useState<number | null>(null);
  const [pickedCourier, setPickedCourier] = useState<string | null>(null);
  const [tab, setTab] = useState<'waiting' | 'road' | 'attention'>('waiting');
  const [radius, setRadius] = useState(2000);
  const [busy, setBusy] = useState('');
  const order = picked !== null ? ord.rows.get(picked) : undefined;
  const nearest = useLive<Near>(conn, order && order.status !== 'dead' ? NEAREST : null, [order?.pickup], ['positions']);
  const within = useLive<{ state: string; n: number }>(conn, order ? WITHIN : null, [order?.pickup, radius], ['positions']);

  const all = [...ord.rows.values()];
  const states = all.map((o) => ({ o, s: shownState(o, now) }));
  const waiting = states.filter(({ s }) => s === 'ready').sort((a, b) => Date.parse(a.o.created) - Date.parse(b.o.created));
  const road = states.filter(({ s }) => s === 'assigned' || s === 'picked').sort((a, b) => Date.parse(a.o.promised) - Date.parse(b.o.promised));
  const attention = states.filter(({ o, s }) => s === 'lapsed' || s === 'dead' || (s !== 'ready' && Date.parse(o.promised) < now) || (s === 'ready' && now - Date.parse(o.created) > 10 * 60_000));
  const list = tab === 'waiting' ? waiting : tab === 'road' ? road : attention;
  // A position past its @ttl is out of every read at once, but a stream
  // hears of it only when the sweeper deletes it, a minute or two later.
  const fleet = [...pos.rows.values()].filter((p) => now - Date.parse(p.seen) < 120_000);
  const count = (s: string) => fleet.filter((p) => p.state === s).length;
  const late = states.filter(({ o, s }) => s !== 'dead' && Date.parse(o.promised) < now).length;

  const act = async (what: string, text: string, params: unknown[]) => {
    setBusy(what);
    try {
      await conn.run(text, params);
    } catch (e) {
      alert((e as Error).message);
    } finally {
      setBusy('');
    }
  };

  const listRef = useRef<HTMLUListElement>(null);
  const focus = order ? svgOf(order.pickup) : null;
  const courierRow = pickedCourier ? fleet.find((p) => p.courier === pickedCourier) : undefined;

  return (
    <>
      <div className="desk">
        <section className="queue" aria-labelledby="queue-h">
          <div className="queue-head">
            <h2 id="queue-h">Orders</h2>
            <p>
              {waiting.length} waiting for a courier, {road.length} on the road
            </p>
            <div className="tabs" role="group" aria-label="Which orders">
              <button type="button" aria-pressed={tab === 'waiting'} onClick={() => setTab('waiting')}>
                Waiting {waiting.length}
              </button>
              <button type="button" aria-pressed={tab === 'road'} onClick={() => setTab('road')}>
                On the road {road.length}
              </button>
              <button type="button" aria-pressed={tab === 'attention'} onClick={() => setTab('attention')}>
                Needs you {attention.length}
              </button>
            </div>
          </div>
          {list.length === 0 ? (
            <p className="empty">{tab === 'waiting' ? 'Every order has a courier.' : tab === 'road' ? 'No order is on the road.' : 'Nothing is late, lapsed or dead.'}</p>
          ) : (
            <ul className="tickets" ref={listRef}>
              {list.slice(0, 200).map(({ o, s }) => {
                const waited = now - Date.parse(o.created);
                return (
                  <li key={o.id}>
                    <button
                      type="button"
                      className="ticket"
                      aria-pressed={picked === o.id}
                      onClick={() => {
                        setPicked(o.id);
                        setPickedCourier(null);
                      }}
                    >
                      <span className="from">{o.pickup_name}</span>
                      <span className={`wait${waited > 10 * 60_000 && s === 'ready' ? ' long' : ''}`}>{since(waited)}</span>
                      <span className="to">to {o.dropoff_addr}</span>
                      <span className={`state ${s}`}>{STATE_WORD[s] ?? s}</span>
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </section>

        <LiveMap base={base} focus={focus} label="Live map of couriers and orders">
          {(k) => (
            <>
              {order && within.length >= 0 && (
                <circle cx={svgOf(order.pickup)[0]} cy={svgOf(order.pickup)[1]} r={radius} className="reach" />
              )}
              {order && (
                <>
                  <path d={`M${svgOf(order.pickup).join(' ')}L${svgOf(order.dropoff).join(' ')}`} className="leg" style={{ strokeWidth: 14 * k }} />
                  <circle cx={svgOf(order.dropoff)[0]} cy={svgOf(order.dropoff)[1]} r={30 * k} className="dropoff-pin" />
                </>
              )}
              {order &&
                nearest.slice(0, 3).map((n) => (
                  <line key={n.courier} x1={svgOf(order.pickup)[0]} y1={svgOf(order.pickup)[1]} x2={svgOf(n.loc)[0]} y2={svgOf(n.loc)[1]} className="nearest-line" style={{ strokeWidth: 6 * k }} />
                ))}
              {states.map(({ o, s }) => {
                const [x, y] = svgOf(o.pickup);
                return (
                  <OrderPin
                    key={o.id}
                    x={x}
                    y={y}
                    k={k}
                    state={s}
                    selected={picked === o.id}
                    title={`${o.pickup_name}: ${STATE_WORD[s] ?? s}`}
                    onPick={() => {
                      setPicked(o.id);
                      setPickedCourier(null);
                    }}
                  />
                );
              })}
              {fleet.map((p) => {
                const [x, y] = svgOf(p.loc);
                const who = people.get(p.courier);
                return (
                  <CourierMark
                    key={p.id}
                    x={x}
                    y={y}
                    k={k}
                    heading={p.heading ?? 0}
                    state={p.state}
                    title={`${who?.name ?? p.courier} (${who?.code ?? ''}): ${STATE_WORD[p.state] ?? p.state}`}
                    onPick={() => {
                      setPickedCourier(p.courier);
                      setPicked(p.job ?? null);
                    }}
                  />
                );
              })}
            </>
          )}
        </LiveMap>

        <aside className="panel" aria-labelledby="panel-h">
          <div className="panel-head">
            <h2 id="panel-h">{order ? order.pickup_name : courierRow ? people.get(courierRow.courier)?.name : 'Select an order'}</h2>
          </div>
          <div className="panel-body">
            {!order && !courierRow && (
              <p className="muted">
                Pick an order in the list or on the map to see the couriers nearest its pickup. Amber tickets wait for a courier; arrows are
                couriers, grey when idle, blue on the way to a pickup, dark blue carrying.
              </p>
            )}
            {courierRow && !order && (
              <dl className="facts">
                <dt>Code</dt>
                <dd>{people.get(courierRow.courier)?.code}</dd>
                <dt>Riding</dt>
                <dd>{courierRow.vehicle ?? people.get(courierRow.courier)?.vehicle}</dd>
                <dt>Now</dt>
                <dd>
                  <span className={`state ${courierRow.state}`}>{STATE_WORD[courierRow.state]}</span>
                </dd>
                <dt>Last seen</dt>
                <dd>{since(now - Date.parse(courierRow.seen))} ago</dd>
              </dl>
            )}
            {order && (
              <>
                <dl className="facts">
                  <dt>Order</dt>
                  <dd>{order.ref}</dd>
                  <dt>Deliver to</dt>
                  <dd>{order.dropoff_addr}</dd>
                  <dt>State</dt>
                  <dd>
                    <span className={`state ${shownState(order, now)}`}>{STATE_WORD[shownState(order, now)] ?? order.status}</span>
                  </dd>
                  <dt>Placed</dt>
                  <dd>{since(now - Date.parse(order.created))} ago</dd>
                  <dt>Promised</dt>
                  <dd>{Date.parse(order.promised) > now ? `in ${since(Date.parse(order.promised) - now)}` : `${since(now - Date.parse(order.promised))} late`}</dd>
                  {order.courier && (
                    <>
                      <dt>Courier</dt>
                      <dd>{people.get(order.courier)?.name ?? order.courier}</dd>
                    </>
                  )}
                  <dt>Attempts</dt>
                  <dd>{order.attempts}</dd>
                  {order.error && (
                    <>
                      <dt>Last error</dt>
                      <dd>{order.error}</dd>
                    </>
                  )}
                </dl>
                <div>
                  <h3 style={{ fontSize: 15, marginBottom: 6 }}>Couriers within reach of the pickup</h3>
                  <div className="radius" role="group" aria-label="Radius">
                    {[1000, 2000, 3000].map((r) => (
                      <button key={r} type="button" aria-pressed={radius === r} onClick={() => setRadius(r)}>
                        {r / 1000} km
                      </button>
                    ))}
                  </div>
                  <p className="split" style={{ marginTop: 8 }}>
                    {['idle', 'pickup', 'dropoff'].map((s) => (
                      <span key={s} className={`state ${s}`}>
                        {within.find((w) => w.state === s)?.n ?? 0} {STATE_WORD[s].toLowerCase()}
                      </span>
                    ))}
                  </p>
                </div>
                {order.status !== 'dead' && (
                  <div>
                    <h3 style={{ fontSize: 15, marginBottom: 4 }}>Nearest idle couriers</h3>
                    {nearest.length === 0 ? (
                      <p className="muted">No idle courier is on the map.</p>
                    ) : (
                      <ol className="nearest">
                        {nearest.map((n) => {
                          const who = people.get(n.courier);
                          const eta = (n.m * 1.3) / (PACE[n.vehicle ?? 'bike'] ?? 4.2);
                          return (
                            <li key={n.courier}>
                              <span>{who?.name ?? n.courier}</span>
                              <span className="eta">{since(eta * 1000)}</span>
                              <span className="sub">
                                {(n.m / 1000).toFixed(2)} km away, {n.vehicle ?? who?.vehicle}
                              </span>
                            </li>
                          );
                        })}
                      </ol>
                    )}
                  </div>
                )}
                <div className="actions">
                  {order.status === 'ready' && (
                    <button
                      className="btn quiet"
                      type="button"
                      disabled={!!busy}
                      onClick={() => act('cancel', 'set orders {status: "cancelled", run_at: null} where id = $1 and status = "ready" require 1', [order.id])}
                    >
                      Cancel order
                    </button>
                  )}
                  {order.status === 'dead' && (
                    <button
                      className="btn"
                      type="button"
                      disabled={!!busy}
                      onClick={() =>
                        act('again', 'set orders {status: "ready", run_at: now(), attempts: 0, error: null, courier: null, claim: null} where id = $1 and status = "dead" require 1', [order.id])
                      }
                    >
                      Send it out again
                    </button>
                  )}
                </div>
              </>
            )}
          </div>
        </aside>
      </div>
      <footer className="strip" aria-label="The fleet now">
        <span>
          <b>{fleet.length}</b>couriers on shift
        </span>
        <span>
          <b>{count('idle')}</b>idle
        </span>
        <span>
          <b>{count('pickup')}</b>to a pickup
        </span>
        <span>
          <b>{count('dropoff')}</b>carrying
        </span>
        <span>
          <b>{waiting.length}</b>orders waiting
        </span>
        <span>
          <b>{late}</b>past their promise
        </span>
        <span className={`live${pos.live && ord.live ? '' : ' down'}`}>{pos.live && ord.live ? 'Live' : 'Reconnecting'}</span>
      </footer>
    </>
  );
}
