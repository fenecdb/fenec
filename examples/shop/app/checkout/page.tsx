import type { Metadata } from 'next';
import { randomBytes } from 'node:crypto';
import { readCart } from '../../lib/cart';
import { shopper } from '../../lib/session';
import { money } from '../../lib/money';
import { HOLD_MS } from '../../lib/checkout';

export const metadata: Metadata = { title: 'Check out', robots: { index: false, follow: false } };

const ERRORS: Record<string, string> = {
  empty: 'Your cart is empty.',
  stock: 'One of the items sold out while you were checking out. Your cart shows what is left; change the quantity and place the order again.',
  changed: 'Your cart changed while the order was being placed. Review it and place the order again.',
  invalid: 'Fill in an email address and every line of the address.',
  failed: 'The order could not be placed. Nothing was charged; try again.',
};

type Props = { searchParams: Promise<{ error?: string }> };

export default async function CheckoutPage({ searchParams }: Props) {
  const { error } = await searchParams;
  const s = await shopper();
  const cart = s ? await readCart(s.owner) : null;
  if (!cart?.lines.length) {
    return (
      <>
        <h1>Check out</h1>
        <p>Your cart is empty. <a href="/">Browse the shop</a>.</p>
      </>
    );
  }
  // The key makes this form's order once: sent twice, it is one order.
  const key = randomBytes(18).toString('base64url');
  return (
    <>
      <h1>Check out</h1>
      {error && ERRORS[error] && <p className="notice bad" role="alert">{ERRORS[error]}</p>}
      <div className="two">
        <form action="/api/checkout" method="post" className="panel">
          <input type="hidden" name="key" value={key} />
          <div className="field"><label htmlFor="email">Email</label><input id="email" name="email" type="email" autoComplete="email" required defaultValue={s?.email ?? ''} /></div>
          <div className="field"><label htmlFor="name">Full name</label><input id="name" name="name" autoComplete="name" required defaultValue={s?.name ?? ''} /></div>
          <div className="field"><label htmlFor="line1">Address</label><input id="line1" name="line1" autoComplete="address-line1" required /></div>
          <div className="field"><label htmlFor="city">Town or city</label><input id="city" name="city" autoComplete="address-level2" required /></div>
          <div className="field"><label htmlFor="postcode">Postcode</label><input id="postcode" name="postcode" autoComplete="postal-code" required /></div>
          <div className="field"><label htmlFor="country">Country</label><input id="country" name="country" autoComplete="country-name" required defaultValue="United States" /></div>
          <p className="muted">Placing the order holds its items for {HOLD_MS / 60000} minutes while you pay.</p>
          <button className="button" type="submit">Place order for {money(cart.total)}</button>
        </form>
        <div>
          <h2>Your order</h2>
          <ul className="lines">
            {cart.lines.map((l) => (
              <li key={l.sku} className="line" style={{ gridTemplateColumns: 'minmax(0,1fr) auto' }}>
                <span>{l.qty} × {l.name}</span>
                <span className="price">{money(l.lineTotal)}</span>
              </li>
            ))}
          </ul>
          <dl className="totals">
            <dt>Items</dt><dd>{money(cart.subtotal)}</dd>
            <dt>Shipping</dt><dd>{cart.shipping ? money(cart.shipping) : 'Free'}</dd>
            <dt className="grand">Total</dt><dd className="grand">{money(cart.total)}</dd>
          </dl>
        </div>
      </div>
    </>
  );
}
