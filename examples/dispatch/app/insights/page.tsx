import type { Metadata } from 'next';
import { TopBar } from '../../components/top-bar';
import { Key, StackedBars } from '../../components/charts';
import { asUser } from '../../lib/db';
import { insights, MERROW_OFFSET } from '../../lib/insights';
import { need, whoOf } from '../../lib/session';

export const metadata: Metadata = { title: 'Insights' };
export const dynamic = 'force-dynamic';

const pct = (a: number, b: number) => (b ? `${Math.round((a / b) * 1000) / 10}%` : 'none yet');
const mins = (ms: number | null) => (ms ? `${Math.round(ms / 60_000)} min` : 'none yet');
const km = (m: number | null) => `${Math.round((m ?? 0) / 1000).toLocaleString('en-GB')} km`;
const hourOf = (iso: string) => new Date(Date.parse(iso) + MERROW_OFFSET).getUTCHours();

export default async function Insights() {
  const a = await need('dispatcher');
  const db = asUser(whoOf(a));
  const r = await insights(db);
  const perDay = r.seven.n / 7;
  return (
    <>
      <TopBar account={a} here="/insights" />
      <main className="page" style={{ maxWidth: 1180 }}>
        <div>
          <h1>Insights</h1>
          <p className="muted">
            Today in Merrow so far, against the seven days before. Each figure is one statement over the orders; this page&apos;s took {r.took.toFixed(0)} ms together.
          </p>
        </div>
        <div className="tiles">
          <div className="tile">
            <p>Delivered today</p>
            <strong>{r.day.n.toLocaleString('en-GB')}</strong>
            <small>{Math.round(perDay).toLocaleString('en-GB')} a day last week</small>
          </div>
          <div className="tile">
            <p>On time today</p>
            <strong>{pct(r.day.on_time, r.day.n)}</strong>
            <small>{pct(r.seven.on_time, r.seven.n)} last week</small>
          </div>
          <div className="tile">
            <p>Order to door</p>
            <strong>{mins(r.day.ms)}</strong>
            <small>{mins(r.seven.ms)} last week</small>
          </div>
          <div className="tile">
            <p>Ridden today</p>
            <strong>{km(r.day.metres)}</strong>
            <small>{km((r.seven.metres ?? 0) / 7)} a day last week</small>
          </div>
        </div>
        <section className="sheet" aria-labelledby="hourly">
          <h2 id="hourly">Deliveries an hour, the last two days</h2>
          <StackedBars
            width={1400}
            height={300}
            title="Deliveries an hour over the last 48 hours, on time and late"
            data={r.hourly.map((h) => ({
              label: new Date(h.hour).toLocaleString('en-GB', { weekday: 'short', hour: '2-digit', minute: '2-digit', timeZone: 'Etc/GMT-3' }),
              tick: hourOf(h.hour) % 6 === 0 ? `${String(hourOf(h.hour)).padStart(2, '0')}:00` : undefined,
              onTime: h.on_time,
              late: h.n - h.on_time,
            }))}
          />
          <Key />
        </section>
        <div className="two">
          <section className="sheet" aria-labelledby="waves">
            <h2 id="waves">The day&apos;s waves, the last two weeks</h2>
            <p className="muted" style={{ fontSize: 14 }}>
              Orders by the hour they were placed. Late ones gather at the top of the dinner rush, when orders outnumber couriers.
            </p>
            <StackedBars
              height={200}
              title="Orders by hour of the day placed, on time and late, the last 14 days"
              data={Array.from({ length: 24 }, (_, h) => {
                const b = r.byHour.find((x) => x.h === h);
                return { label: `${String(h).padStart(2, '0')}:00`, tick: h % 3 === 0 ? String(h) : undefined, onTime: b?.on_time ?? 0, late: (b?.n ?? 0) - (b?.on_time ?? 0) };
              })}
            />
            <Key />
          </section>
          <section className="sheet" aria-labelledby="leaders">
            <h2 id="leaders">Most deliveries, the last seven days</h2>
            <table className="board">
              <thead>
                <tr>
                  <th>Courier</th>
                  <th className="n">Delivered</th>
                  <th className="n">On time</th>
                  <th className="n">Ridden</th>
                  <th className="n">Ride</th>
                </tr>
              </thead>
              <tbody>
                {r.leaders.map((l) => (
                  <tr key={l.courier}>
                    <td>
                      {l.couriers[0]?.name ?? l.courier} <span className="muted">{l.couriers[0]?.code}</span>
                    </td>
                    <td className="n">{l.n}</td>
                    <td className="n">{pct(l.on_time, l.n)}</td>
                    <td className="n">{km(l.metres)}</td>
                    <td className="n">{mins(l.ride)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </section>
        </div>
      </main>
    </>
  );
}
