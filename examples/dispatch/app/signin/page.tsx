import type { Metadata } from 'next';
import { redirect } from 'next/navigation';
import { MapBase, VIEWBOX } from '../../components/map-base';
import { Shield } from '../../components/top-bar';
import { current, home } from '../../lib/session';
import { demoAccounts } from '../../lib/demo';
import { DEMO_PASSWORD } from '../../lib/world';

export const metadata: Metadata = { title: 'Sign in' };
export const dynamic = 'force-dynamic';

const ROLE = { dispatcher: 'Dispatcher', courier: 'Courier', customer: 'Customer' } as const;

export default async function SignIn({ searchParams }: { searchParams: Promise<{ failed?: string }> }) {
  const a = await current();
  if (a) redirect(home(a));
  const { failed } = await searchParams;
  const demo = demoAccounts();
  return (
    <div className="signin">
      <div className="signin-map">
        <svg viewBox={VIEWBOX} preserveAspectRatio="xMidYMid slice" role="img" aria-label="A street map of Merrow">
          <MapBase />
        </svg>
      </div>
      <main className="signin-panel">
        <a className="mark" href="/" style={{ color: 'var(--asphalt)' }}>
          <Shield size={34} />
          Plover
        </a>
        <div>
          <h1>Every order to the nearest courier</h1>
          <p className="lede">Dispatch for Merrow&apos;s delivery companies. Sign in to your company&apos;s map, your shift, or your orders.</p>
        </div>
        <form className="form" action="/api/signin" method="post">
          <div className="field">
            <label htmlFor="email">Email</label>
            <input id="email" name="email" type="email" autoComplete="username" required />
          </div>
          <div className="field">
            <label htmlFor="password">Password</label>
            <input id="password" name="password" type="password" autoComplete="current-password" required />
          </div>
          {failed && <p className="error" role="alert">That email and password do not match an account.</p>}
          <button className="btn" type="submit">
            Sign in
          </button>
        </form>
        <section className="demo" aria-labelledby="demo">
          <h2 id="demo">Try it as someone</h2>
          <p className="muted" style={{ fontSize: 14, marginBottom: 10 }}>
            Every demo account&apos;s password is &ldquo;{DEMO_PASSWORD}&rdquo;. Emails match whatever their case.
          </p>
          <ul>
            {demo.map((d) => (
              <li key={d.email}>
                <span>
                  {d.name} <span className="role">{ROLE[d.role]}, {d.companyName}</span>
                </span>
                <form action="/api/signin" method="post">
                  <input type="hidden" name="email" value={d.email} />
                  <input type="hidden" name="password" value={DEMO_PASSWORD} />
                  <button className="btn quiet" type="submit">
                    Sign in
                  </button>
                </form>
              </li>
            ))}
          </ul>
        </section>
      </main>
    </div>
  );
}
