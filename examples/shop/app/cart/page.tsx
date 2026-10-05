import type { Metadata } from 'next';
import { readCart, CART_ERRORS, MAX_QTY, type CartErrorCode } from '../../lib/cart';
import { shopper } from '../../lib/session';
import { ProductArt } from '../../components/art';
import { CATEGORY_SHAPES } from '../../lib/category-shapes';
import { money, FREE_SHIPPING_FROM } from '../../lib/money';

export const metadata: Metadata = { title: 'Your cart', robots: { index: false, follow: false } };

type Props = { searchParams: Promise<{ error?: string; added?: string }> };

export default async function CartPage({ searchParams }: Props) {
  const sp = await searchParams;
  const s = await shopper();
  const cart = s ? await readCart(s.owner) : { lines: [], count: 0, subtotal: 0, shipping: 0, total: 0 };
  const error = sp.error && sp.error in CART_ERRORS ? CART_ERRORS[sp.error as CartErrorCode] : null;
  return (
    <>
      <h1>Your cart</h1>
      {error && <p className="notice bad" role="alert">{error}</p>}
      {sp.added && !error && <p className="notice" role="status">Added to your cart.</p>}
      {cart.lines.length === 0 ? (
        <>
          <p>Your cart is empty. Shade, water and light are a good place to start.</p>
          <p><a className="button" href="/">Browse the shop</a></p>
        </>
      ) : (
        <div className="two">
          <ul className="lines">
            {cart.lines.map((l) => (
              <li key={l.sku} className="line">
                <ProductArt shape={CATEGORY_SHAPES[l.category] ?? 'cube'} colour={l.colour} sku={l.sku} />
                <div>
                  <a href={`/p/${l.slug}`}><strong>{l.name}</strong></a>
                  <div className="muted">{money(l.price)} each{l.available <= 0 ? ', sold out: take it out to check out' : l.available < l.qty ? `, only ${l.available} left` : ''}</div>
                  <div className="line-controls">
                    <form action="/api/cart" method="post">
                      <input type="hidden" name="action" value="set" />
                      <input type="hidden" name="sku" value={l.sku} />
                      <label className="sr" htmlFor={`q-${l.sku}`}>Quantity of {l.name}</label>
                      <select id={`q-${l.sku}`} name="qty" className="qty" defaultValue={String(l.qty)}>
                        {Array.from({ length: MAX_QTY }, (_, i) => <option key={i + 1} value={i + 1}>{i + 1}</option>)}
                      </select>{' '}
                      <button className="link-button" type="submit">Update</button>
                    </form>
                    <form action="/api/cart" method="post">
                      <input type="hidden" name="action" value="remove" />
                      <input type="hidden" name="sku" value={l.sku} />
                      <button className="link-button" type="submit">Remove</button>
                    </form>
                  </div>
                </div>
                <span className="price">{money(l.lineTotal)}</span>
              </li>
            ))}
          </ul>
          <div className="panel">
            <dl className="totals">
              <dt>Items ({cart.count})</dt><dd>{money(cart.subtotal)}</dd>
              <dt>Shipping</dt><dd>{cart.shipping ? money(cart.shipping) : 'Free'}</dd>
              <dt className="grand">Total</dt><dd className="grand">{money(cart.total)}</dd>
            </dl>
            {cart.shipping > 0 && <p className="muted">Add {money(FREE_SHIPPING_FROM - cart.subtotal)} more for free shipping.</p>}
            <a className="button" href="/checkout" style={{ width: '100%' }}>Check out</a>
          </div>
        </div>
      )}
    </>
  );
}
