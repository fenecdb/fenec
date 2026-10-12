import type { Metadata } from 'next';
import { TopBar } from '../../components/top-bar';
import { asUser, rows } from '../../lib/db';
import { need, whoOf } from '../../lib/session';
import { COMPANIES, people } from '../../lib/world';
import type { Job } from '../../lib/dispatch';
import { serverNow, shownState } from '../../lib/states';

export const metadata: Metadata = { title: 'My orders' };
export const dynamic = 'force-dynamic';

const WORD: Record<string, string> = {
  ready: 'Finding a courier',
  assigned: 'Courier on the way to the shop',
  picked: 'On its way to you',
  lapsed: 'Finding another courier',
  delivered: 'Delivered',
  cancelled: 'Cancelled',
  dead: 'Held up: the company will call you',
};

export default async function Orders() {
  const a = await need('customer');
  const db = asUser(whoOf(a));
  const me = people(COMPANIES.find((c) => c.slug === a.company)!).find((p) => p.uid === a.uid)!;
  const [mine, near] = await Promise.all([
    // A customer's token reads their own orders whatever the filter says;
    // the filter is for the index.
    rows<Job>(db, 'get orders where customer = $1 order created desc limit 20', [a.uid]),
    rows<{ code: string; name: string; kind: string; address: string; m: number }>(
      db,
      'get merchants select code, name, kind, address, distance(loc, $1) as m near loc $1 limit 8',
      [me.home.point],
    ),
  ]);
  const now = serverNow();
  return (
    <>
      <TopBar account={a} here="/orders" />
      <main className="page">
        <h1>Hello, {a.name.split(' ')[0]}</h1>
        <div className="two">
          <section className="sheet" aria-labelledby="mine">
            <h2 id="mine">Your orders</h2>
            {mine.length === 0 ? (
              <p className="muted">Nothing yet. Order from a shop near you, and follow its courier here.</p>
            ) : (
              <ul className="orders-list">
                {mine.map((o) => {
                  const s = shownState(o, now);
                  return (
                    <li key={o.id}>
                      <a href={`/orders/${o.id}`}>
                        <strong>{o.pickup_name}</strong>
                        <span className={`state ${s}`}>{WORD[s] ?? s}</span>
                        <small>
                          {new Date(o.created).toLocaleString('en-GB', { day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit', timeZone: 'Etc/GMT-3' })}, to{' '}
                          {o.dropoff_addr}
                        </small>
                      </a>
                    </li>
                  );
                })}
              </ul>
            )}
          </section>
          <section className="sheet" aria-labelledby="near">
            <h2 id="near">Order from near you</h2>
            <p className="muted" style={{ marginBottom: 10, fontSize: 14 }}>
              Delivered to {me.home.label}.
            </p>
            <ul className="shops">
              {near.map((m) => (
                <li key={m.code}>
                  <span>{m.name}</span>
                  <small>
                    {m.address}, {m.m < 1000 ? `${Math.round(m.m)} m` : `${(m.m / 1000).toFixed(1)} km`} away
                  </small>
                  <form action="/api/orders" method="post">
                    <input type="hidden" name="merchant" value={m.code} />
                    <button className="btn quiet" type="submit">
                      Order
                    </button>
                  </form>
                </li>
              ))}
            </ul>
          </section>
        </div>
      </main>
    </>
  );
}
