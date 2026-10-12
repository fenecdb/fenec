// The bar every signed-in page opens with: the mark, the company, the
// pages this person's role has, and who they are.
import type { Account } from '../lib/session';
import { COMPANIES } from '../lib/world';

export function Shield({ size = 26 }: { size?: number }) {
  // A route shield, as the signs on Merrow's arterials carry their numbers.
  return (
    <svg width={size} height={size} viewBox="0 0 32 32" aria-hidden="true">
      <path d="M4 3h24c1 3 1 6-1 9 3 5 3 11-1 15-3 2-7 3-10 4-3-1-7-2-10-4C2 23 2 17 5 12 3 9 3 6 4 3z" fill="#ffcc33" />
      <path d="M12 23V9h5.4c3 0 4.8 1.7 4.8 4.4S20.4 18 17.4 18H15.3v5z" fill="#22303b" />
      <path d="M15.3 11.6v3.8h1.9c1.2 0 1.9-.7 1.9-1.9s-.7-1.9-1.9-1.9z" fill="#ffcc33" />
    </svg>
  );
}

const PAGES: Record<Account['role'], { href: string; label: string }[]> = {
  dispatcher: [
    { href: '/dispatch', label: 'Live map' },
    { href: '/insights', label: 'Insights' },
  ],
  courier: [{ href: '/courier', label: 'My shift' }],
  customer: [{ href: '/orders', label: 'My orders' }],
};

export function TopBar({ account, here }: { account: Account; here: string }) {
  const company = COMPANIES.find((c) => c.slug === account.company);
  return (
    <header className="bar">
      <a className="mark" href="/">
        <Shield />
        Plover
      </a>
      {company && <span className="company">{company.name}</span>}
      <nav aria-label="Pages">
        {PAGES[account.role].map((p) => (
          <a key={p.href} href={p.href} aria-current={p.href === here ? 'page' : undefined}>
            {p.label}
          </a>
        ))}
      </nav>
      <div className="who">
        <span>{account.name}</span>
        <form action="/api/signout" method="post">
          <button className="link-button" type="submit">
            Sign out
          </button>
        </form>
      </div>
    </header>
  );
}
