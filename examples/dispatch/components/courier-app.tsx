'use client';
// A courier's phone. On shift, it holds a claim at the server for the
// nearest order within 3 km (the request waits there until one comes), and
// beats every 2.5 s: its position, and the lease of the order it carries.
// "Ride for me" moves it along the streets, since whoever is looking at
// this is not on a bike.
import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { along, route, svgOf, pathOf, type Point } from '../lib/city';
import type { Job, Offer } from '../lib/dispatch';
import { CourierMark, LiveMap, OrderPin } from './live-map';

export interface Delivery {
  id: number;
  pickup_name: string;
  dropoff_addr: string;
  fee: number;
  delivered_at: string;
  driven: number | null;
}

const PACE: Record<string, number> = { bike: 4.2, scooter: 6.5, car: 7.5 };
const SPEED = 6; // the demo's: six times a real ride
const BEAT_MS = 2500;
const money = (cents: number) => `€${(cents / 100).toFixed(2)}`;
/** A time as Merrow's clocks show it: UTC+3, 24 hours. */
export const clock = (iso: string) => new Date(iso).toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit', timeZone: 'Etc/GMT-3' });

async function post<T>(path: string, body: object, signal?: AbortSignal): Promise<T> {
  const res = await fetch(`/api/courier/${path}`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body), signal });
  const out = await res.json();
  if (!res.ok) throw Object.assign(new Error(out.error ?? `HTTP ${res.status}`), { status: res.status, retryAfter: out.retryAfter });
  return out as T;
}

export function CourierApp({
  base,
  name,
  vehicle,
  start,
  carrying,
  recent,
  today,
}: {
  base: ReactNode;
  name: string;
  vehicle: string;
  start: Point;
  carrying: Job | null;
  recent: Delivery[];
  today: { n: number; fees: number; metres: number };
}) {
  const [online, setOnline] = useState(!!carrying);
  const [job, setJob] = useState<Job | null>(carrying);
  const [at, setAt] = useState<Point>(start);
  const [heading, setHeading] = useState(0);
  const [ride, setRide] = useState(true);
  const [notice, setNotice] = useState<{ text: string; bad?: boolean } | null>(null);
  const [done, setDone] = useState(recent);
  const [tally, setTally] = useState(today);
  const [path, setPath] = useState<Point[]>(() => (carrying ? route(start, carrying.status === 'picked' ? carrying.dropoff : carrying.pickup).path : []));
  const travelled = useRef(0);
  const legs = useRef(0);
  // What the beat and the claim read when they run: the latest, not what
  // their effect saw when it began.
  const live = useRef({ at, heading, job });
  useEffect(() => {
    live.current = { at, heading, job };
  });

  const phase = !online ? 'off' : !job ? 'looking' : job.status === 'picked' ? 'dropoff' : 'pickup';

  // While looking, the ready orders farther out than a claim reaches: where
  // to ride to. Read under the courier's own token, which shows a ready
  // order's pickup and fee and not its customer's door.
  const [offers, setOffers] = useState<Offer[]>([]);
  useEffect(() => {
    if (phase !== 'looking') return;
    let stop = false;
    const look = async () => {
      try {
        const r = await post<{ orders: Offer[] }>('nearby', { at: live.current.at });
        if (!stop) setOffers(r.orders);
      } catch {
        // The next look says it again.
      }
    };
    look();
    const t = setInterval(look, 4 * BEAT_MS);
    return () => {
      stop = true;
      clearInterval(t);
    };
  }, [phase]);
  const near = phase === 'looking' ? offers : [];

  // The beat: position and lease, every 2.5 s on shift.
  useEffect(() => {
    if (!online) return;
    let stop = false;
    const beat = async () => {
      const { at, heading, job } = live.current;
      try {
        const r = await post<{ kept: boolean; throttled?: number }>('beat', {
          at,
          heading,
          vehicle,
          state: !job ? 'idle' : job.status === 'picked' ? 'dropoff' : 'pickup',
          job: job ? { id: job.id, claim: job.claim } : null,
        });
        if (stop) return;
        if (!r.kept) {
          setJob(null);
          setPath([]);
          setNotice({ text: 'That order went to another courier: your phone was silent past its lease.', bad: true });
        }
      } catch {
        // The next beat says it again.
      }
    };
    beat();
    const t = setInterval(beat, BEAT_MS);
    return () => {
      stop = true;
      clearInterval(t);
    };
  }, [online, vehicle]);

  // The claim, held at the server while nothing near is ready.
  useEffect(() => {
    if (!online || job) return;
    const abort = new AbortController();
    (async () => {
      while (!abort.signal.aborted) {
        try {
          const r = await post<{ job: Job | null }>('claim', { at: live.current.at }, abort.signal);
          if (r.job) {
            setJob(r.job);
            setPath(route(live.current.at, r.job.pickup).path);
            travelled.current = 0;
            legs.current = 0;
            setNotice(null);
            return;
          }
        } catch (e) {
          if (abort.signal.aborted) return;
          const wait = ((e as { retryAfter?: number }).retryAfter ?? 2) * 1000;
          setNotice({ text: `Could not ask for orders: ${(e as Error).message}. Trying again.`, bad: true });
          await new Promise((r) => setTimeout(r, wait));
        }
      }
    })();
    return () => abort.abort();
  }, [online, job]);

  // Ride for me: along the route at the vehicle's pace, six times over.
  useEffect(() => {
    if (!ride || !path.length) return;
    const t = setInterval(() => {
      travelled.current += (PACE[vehicle] ?? 4.2) * SPEED * 0.25;
      const step = along(path, travelled.current);
      setAt(step.at);
      setHeading(step.heading);
    }, 250);
    return () => clearInterval(t);
  }, [ride, path, vehicle]);

  const pickedUp = useCallback(async () => {
    if (!job) return;
    const r = await post<{ ok: boolean }>('pickup', { id: job.id, claim: job.claim });
    if (!r.ok) {
      setJob(null);
      setPath([]);
      setNotice({ text: 'That order is no longer yours: its lease lapsed and another courier took it.', bad: true });
      return;
    }
    legs.current += route(at, job.pickup).metres;
    setJob({ ...job, status: 'picked' });
    setPath(route(at, job.dropoff).path);
    travelled.current = 0;
  }, [job, at]);

  const delivered = useCallback(async () => {
    if (!job) return;
    const driven = legs.current + route(job.pickup, job.dropoff).metres;
    const r = await post<{ ok: boolean }>('deliver', { id: job.id, claim: job.claim, driven });
    setJob(null);
    setPath([]);
    if (!r.ok) {
      setNotice({ text: 'That order is no longer yours: its lease lapsed and another courier took it.', bad: true });
      return;
    }
    setNotice({ text: `Delivered to ${job.dropoff_addr}. ${money(job.fee)} earned.` });
    setDone((d) => [{ id: job.id, pickup_name: job.pickup_name, dropoff_addr: job.dropoff_addr, fee: job.fee, delivered_at: new Date().toISOString(), driven }, ...d].slice(0, 10));
    setTally((t) => ({ n: t.n + 1, fees: t.fees + job.fee, metres: t.metres + driven }));
  }, [job]);

  const handBack = useCallback(async () => {
    if (!job) return;
    await post('giveback', { id: job.id, claim: job.claim, why: 'the courier could not carry it' });
    setJob(null);
    setPath([]);
    setNotice({ text: 'Given back: another courier will be offered it.' });
  }, [job]);

  const [x, y] = svgOf(at);
  const view: [number, number, number, number] = [x - 900, y - 1100, 1800, 2200];
  return (
    <>
      <div className="phone-map">
        <LiveMap base={base} initial={view} focus={[x, y]} label="Your position and your next stop" controls={false}>
          {(k) => (
            <>
              {path.length > 0 && <path d={pathOf(path)} className="route" style={{ strokeWidth: 40 * k }} />}
              {near.map((o) => (
                <OrderPin key={o.id} x={svgOf(o.pickup)[0]} y={svgOf(o.pickup)[1]} k={k * 1.4} state="ready" title={`${o.pickup_name}, ${money(o.fee)}`} />
              ))}
              {job && phase === 'pickup' && <OrderPin x={svgOf(job.pickup)[0]} y={svgOf(job.pickup)[1]} k={k * 2} state="ready" title={job.pickup_name} />}
              {job && <circle cx={svgOf(job.dropoff)[0]} cy={svgOf(job.dropoff)[1]} r={60 * k} className="dropoff-pin" />}
              <CourierMark x={x} y={y} k={k * 2.4} heading={heading} state={phase === 'off' || phase === 'looking' ? 'idle' : phase} title={name} />
            </>
          )}
        </LiveMap>
      </div>
      <section className="job" aria-live="polite">
        {notice && <p className={`notice${notice.bad ? ' bad' : ''}`}>{notice.text}</p>}
        {phase === 'off' && (
          <>
            <h1>Off shift</h1>
            <p className="muted">Go on shift to be offered the nearest order within 3 km of you.</p>
          </>
        )}
        {phase === 'looking' && (
          <div className="waiting">
            <h1>Looking for orders</h1>
            <p className="muted">The nearest order within 3 km comes to you the moment it is placed.</p>
            {near.length > 0 && (
              <p className="muted">
                {near.length} ready farther out, the nearest at {near[0].pickup_name}, {(near[0].metres / 1000).toFixed(1)} km away, {money(near[0].fee)}.
              </p>
            )}
          </div>
        )}
        {job && (
          <>
            <h1>{phase === 'pickup' ? `Pick up at ${job.pickup_name}` : `Deliver to ${job.dropoff_addr}`}</h1>
            <div className="leg-step">
              <span className="dot from" />
              <strong>{job.pickup_name}</strong>
              <span>{job.items} {job.items === 1 ? 'item' : 'items'}, order {job.ref}</span>
            </div>
            <div className="leg-step">
              <span className="dot to" />
              <strong>{job.dropoff_addr}</strong>
              <span>{money(job.fee)} for this delivery, due by {clock(job.promised)}</span>
            </div>
            {phase === 'pickup' ? (
              <button className="btn big" type="button" onClick={pickedUp}>
                Picked up
              </button>
            ) : (
              <button className="btn big go" type="button" onClick={delivered}>
                Delivered
              </button>
            )}
            <button className="btn quiet" type="button" onClick={handBack}>
              I can&apos;t carry this one
            </button>
          </>
        )}
        <div className="toggle">
          <div>
            <strong>On shift</strong>
            <span>{online ? 'Your position is shared with dispatch' : 'Nobody sees where you are'}</span>
          </div>
          <button type="button" role="switch" className="switch" aria-checked={online} aria-label="On shift" disabled={!!job} onClick={() => setOnline((o) => !o)} />
        </div>
        <div className="toggle">
          <div>
            <strong>Ride for me</strong>
            <span>The demo moves you along the streets</span>
          </div>
          <button type="button" role="switch" className="switch" aria-checked={ride} aria-label="Ride for me" onClick={() => setRide((r) => !r)} />
        </div>
      </section>
      <section className="history" aria-labelledby="today">
        <h2 id="today">
          Today: {tally.n} {tally.n === 1 ? 'delivery' : 'deliveries'}, {money(tally.fees)}, {(tally.metres / 1000).toFixed(1)} km
        </h2>
        {done.length === 0 ? (
          <p className="muted">Your deliveries show here.</p>
        ) : (
          <ol>
            {done.map((d) => (
              <li key={d.id}>
                <span>{d.pickup_name}</span>
                <span>{money(d.fee)}</span>
                <small>
                  to {d.dropoff_addr}, {clock(d.delivered_at)}
                </small>
              </li>
            ))}
          </ol>
        )}
      </section>
    </>
  );
}
