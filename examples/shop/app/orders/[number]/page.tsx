import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { orderOf } from '../../../lib/orders';
import { shopper } from '../../../lib/session';
import { money } from '../../../lib/money';

export const metadata: Metadata = { title: 'Your order', robots: { index: false, follow: false } };

const ERRORS: Record<string, string> = {
  declined: 'The card was declined, so nothing was charged and the items went back on sale. Add them to your cart again to retry.',
  gone: 'This order’s reservation lapsed before it was paid; nothing was charged.',
  failed: 'The payment could not be recorded. Nothing was charged; try again.',
};

type Props = { params: Promise<{ number: string }>; searchParams: Promise<{ error?: string; paid?: string }> };

export default async function OrderPage({ params, searchParams }: Props) {
  const { number } = await params;
  const sp = await searchParams;
  const s = await shopper();
  // Read under the shopper's own token: another shopper's number is not found.
  const o = s ? await orderOf(s.owner, number) : null;
  if (!o) notFound();
  return (
    <>
      <h1>Order {o.number}</h1>
      <p>
        <span className={`status ${o.status}`}>{o.status === 'reserved' ? 'Awaiting payment' : o.status === 'paid' ? 'Paid' : 'Cancelled'}</span>
      </p>
      {sp.error && ERRORS[sp.error] && <p className="notice bad" role="alert">{ERRORS[sp.error]}</p>}
      {o.status === 'paid' && <p className="notice" role="status">Thank you. We will email {o.email} when it ships.</p>}
      <div className="two">
        <div>
          <ul className="lines">
            {o.lines.map((l) => (
              <li key={l.sku} className="line" style={{ gridTemplateColumns: 'minmax(0,1fr) auto' }}>
                <span>{l.qty} × {l.name}</span>
                <span className="price">{money(l.qty * l.price)}</span>
              </li>
            ))}
          </ul>
          <dl className="totals">
            <dt>Items</dt><dd>{money(o.subtotal)}</dd>
            <dt>Shipping</dt><dd>{o.shipping ? money(o.shipping) : 'Free'}</dd>
            <dt className="grand">Total</dt><dd className="grand">{money(o.total)}</dd>
          </dl>
        </div>
        {o.status === 'reserved' && (
          <form action="/api/pay" method="post" className="panel">
            <h2>Pay</h2>
            <p className="muted">
              Your items are held until {o.holdUntil ? new Date(o.holdUntil).toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit', timeZone: 'UTC' }) + ' UTC' : 'the hold ends'}.
              This shop takes test cards only: 4242 4242 4242 4242 is approved, one ending in 0002 is declined.
            </p>
            <input type="hidden" name="number" value={o.number} />
            <div className="field">
              <label htmlFor="card">Card number</label>
              <input id="card" name="card" inputMode="numeric" autoComplete="cc-number" required defaultValue="4242 4242 4242 4242" />
            </div>
            <button className="button" type="submit">Pay {money(o.total)}</button>
          </form>
        )}
      </div>
    </>
  );
}
