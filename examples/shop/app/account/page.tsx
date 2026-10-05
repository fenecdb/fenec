import type { Metadata } from 'next';
import { shopper } from '../../lib/session';
import { ordersOf } from '../../lib/orders';
import { money } from '../../lib/money';

export const metadata: Metadata = { title: 'Your account', robots: { index: false, follow: false } };

const ERRORS: Record<string, string> = {
  invalid: 'Enter an email address and a password of at least 8 characters.',
  taken: 'There is already an account with that email. Sign in instead.',
  wrong: 'That email and password do not match an account.',
};

type Props = { searchParams: Promise<{ error?: string }> };

export default async function AccountPage({ searchParams }: Props) {
  const { error } = await searchParams;
  const s = await shopper();
  if (s?.userId) {
    const list = await ordersOf(s.owner);
    return (
      <>
        <h1>Hello, {s.name}</h1>
        <h2>Your orders</h2>
        {list.length === 0 ? (
          <p>No orders yet.</p>
        ) : (
          <ul className="lines">
            {list.map((o) => (
              <li key={o.number} className="line" style={{ gridTemplateColumns: 'minmax(0,1fr) auto auto' }}>
                <a href={`/orders/${o.number}`}>{o.number}</a>
                <span className={`status ${o.status}`}>{o.status}</span>
                <span className="price">{money(o.total)}</span>
              </li>
            ))}
          </ul>
        )}
        <form action="/api/account" method="post">
          <input type="hidden" name="action" value="signout" />
          <button className="button quiet" type="submit">Sign out</button>
        </form>
      </>
    );
  }
  return (
    <>
      <h1>Your account</h1>
      {error && ERRORS[error] && <p className="notice bad" role="alert">{ERRORS[error]}</p>}
      <p className="muted">Sign in to keep your cart across devices and see your orders. You can also check out as a guest.</p>
      <div className="two">
        <form action="/api/account" method="post" className="panel">
          <h2>Sign in</h2>
          <input type="hidden" name="action" value="signin" />
          <div className="field"><label htmlFor="si-email">Email</label><input id="si-email" name="email" type="email" autoComplete="email" required /></div>
          <div className="field"><label htmlFor="si-password">Password</label><input id="si-password" name="password" type="password" autoComplete="current-password" required /></div>
          <button className="button" type="submit">Sign in</button>
        </form>
        <form action="/api/account" method="post" className="panel">
          <h2>Create an account</h2>
          <input type="hidden" name="action" value="signup" />
          <div className="field"><label htmlFor="su-name">Name</label><input id="su-name" name="name" autoComplete="name" /></div>
          <div className="field"><label htmlFor="su-email">Email</label><input id="su-email" name="email" type="email" autoComplete="email" required /></div>
          <div className="field"><label htmlFor="su-password">Password, 8 characters or more</label><input id="su-password" name="password" type="password" autoComplete="new-password" minLength={8} required /></div>
          <button className="button quiet" type="submit">Create account</button>
        </form>
      </div>
    </>
  );
}
