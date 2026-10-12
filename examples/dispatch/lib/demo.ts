// The accounts the sign-in page offers, one a role a company. The last
// courier of each fleet is kept off the simulator's roster, so whoever signs
// in as them rides alone.
import { COMPANIES, people, type Person } from './world';

export interface DemoAccount {
  company: string;
  companyName: string;
  role: Person['role'];
  name: string;
  /** Typed as a person might: the case differs from the stored address. */
  email: string;
}

const typed = (email: string) => {
  const [local, domain] = email.split('@');
  const name = local.split('.').map((w) => w[0].toUpperCase() + w.slice(1)).join('.');
  return `${name}@${domain[0].toUpperCase()}${domain.slice(1)}`;
};

export function demoAccounts(): DemoAccount[] {
  const out: DemoAccount[] = [];
  for (const c of COMPANIES) {
    const folk = people(c);
    const pick = [
      folk.find((p) => p.role === 'dispatcher')!,
      folk.filter((p) => p.role === 'courier').at(-1)!,
      folk.find((p) => p.role === 'customer')!,
    ];
    for (const p of pick) out.push({ company: c.slug, companyName: c.name, role: p.role, name: p.name, email: typed(p.email) });
  }
  return out;
}

/** The courier a person drives in the demo, whom the simulator leaves alone. */
export const demoCourier = (slug: string) => people(COMPANIES.find((c) => c.slug === slug)!).filter((p) => p.role === 'courier').at(-1)!;
