// Who and what is in Merrow: the delivery companies on Plover, their
// dispatchers, couriers, customers and the merchants they pick up from --
// all invented, all made from seeds, so every run has the same people at
// the same doors.
import { addresses, rng, type Address, type Point } from './city';
import type { Plan } from './plans';

export interface CompanySpec {
  slug: string;
  name: string;
  plan: Plan;
  couriers: number;
  dispatchers: number;
  customers: number;
  merchants: number;
  /** Orders a day at its busiest, for the history and the simulation. */
  ordersPerDay: number;
  seed: number;
}

export const COMPANIES: CompanySpec[] = [
  { slug: 'tandem', name: 'Tandem Couriers', plan: 'pro', couriers: 60, dispatchers: 3, customers: 400, merchants: 150, ordersPerDay: 900, seed: 11 },
  { slug: 'hopper', name: 'Hopper & Daughters', plan: 'starter', couriers: 20, dispatchers: 1, customers: 150, merchants: 60, ordersPerDay: 260, seed: 23 },
];

/** Every seeded account's password: a demo's, never a deployment's. */
export const DEMO_PASSWORD = 'plover demo password';

const FIRST = [
  'Ada', 'Bram', 'Cleo', 'Dario', 'Esme', 'Femi', 'Greta', 'Hugo', 'Ines', 'Jonah', 'Kemal', 'Lidia', 'Milo', 'Nadia',
  'Oskar', 'Priya', 'Quinn', 'Rosa', 'Sami', 'Tova', 'Umar', 'Vera', 'Wren', 'Xavi', 'Yusuf', 'Zora', 'Aylin', 'Bastian',
  'Chidi', 'Defne', 'Elif', 'Farid', 'Gaia', 'Hana', 'Ilya', 'Jun', 'Kaia', 'Luca', 'Mira', 'Noor', 'Orla', 'Pavel',
  'Rafa', 'Selin', 'Teo', 'Una', 'Viggo', 'Wanda', 'Yara', 'Zeki',
];
const LAST = [
  'Lark', 'Okafor', 'Brandt', 'Moreau', 'Kaya', 'Lindqvist', 'Ferreira', 'Novak', 'Haddad', 'Sato', 'Petrov', 'Quist',
  'Romano', 'Demir', 'Achebe', 'Varga', 'Holm', 'Castell', 'Ibarra', 'Mendes', 'Yilmaz', 'Strand', 'Kowal', 'Arslan',
  'Bauer', 'Coelho', 'Dunmore', 'Erdem', 'Falk', 'Grieve', 'Hale', 'Ishikawa', 'Jovic', 'Keane', 'Lowe', 'Marsh',
];
const KINDS = ['Pizzeria', 'Noodle bar', 'Bakery', 'Grocer', 'Pharmacy', 'Florist', 'Taqueria', 'Deli', 'Curry house', 'Bookshop', 'Burger joint', 'Sushi bar'];
const ADJ = ['Copper', 'Little', 'Salt', 'Golden', 'Blue', 'Old', 'Lantern', 'Wild', 'Corner', 'Harbour', 'Juniper', 'Quay', 'Sparrow', 'Velvet', 'Mill', 'North'];
const NOUN = ['Kettle', 'Fig', 'Anchor', 'Oven', 'Spoon', 'Lemon', 'Barrel', 'Pepper', 'Basket', 'Gate', 'Crust', 'Wharf', 'Bell', 'Tide', 'Leaf', 'Hearth'];

export interface Person {
  uid: string;
  name: string;
  email: string;
  role: 'dispatcher' | 'courier' | 'customer';
  code: string | null;
  vehicle?: 'bike' | 'scooter' | 'car';
  /** A customer's door, a courier's home base. */
  home: Address;
}

export interface MerchantSpec {
  code: string;
  name: string;
  kind: string;
  address: string;
  loc: Point;
}

const pick = <T>(r: () => number, xs: T[]) => xs[Math.floor(r() * xs.length)];

/** The people of a company: its dispatchers, couriers and customers. */
export function people(c: CompanySpec): Person[] {
  const r = rng(c.seed * 7919);
  const doors = addresses();
  const out: Person[] = [];
  const domain = `${c.slug}.example`;
  const used = new Set<string>();
  const name = () => {
    for (;;) {
      const n = `${pick(r, FIRST)} ${pick(r, LAST)}`;
      if (!used.has(n)) {
        used.add(n);
        return n;
      }
    }
  };
  const mail = (n: string, at: string) => `${n.toLowerCase().replace(/ /g, '.')}@${at}`;
  for (let i = 1; i <= c.dispatchers; i++) {
    const n = name();
    out.push({ uid: `d-${c.slug}-${i}`, name: n, email: mail(n, domain), role: 'dispatcher', code: `D-${String(i).padStart(3, '0')}`, home: pick(r, doors) });
  }
  for (let i = 1; i <= c.couriers; i++) {
    const n = name();
    const v = r();
    out.push({
      uid: `c-${c.slug}-${i}`,
      name: n,
      email: mail(n, domain),
      role: 'courier',
      code: `C-${String(i).padStart(3, '0')}`,
      vehicle: v < 0.55 ? 'bike' : v < 0.85 ? 'scooter' : 'car',
      home: pick(r, doors),
    });
  }
  for (let i = 1; i <= c.customers; i++) {
    const n = `${pick(r, FIRST)} ${pick(r, LAST)}`;
    out.push({ uid: `k-${c.slug}-${i}`, name: n, email: mail(n, `${i}.${c.slug}-customers.example`), role: 'customer', code: null, home: pick(r, doors) });
  }
  return out;
}

/**
 * A company's merchants: most in the Old Town and along the main roads,
 * where a city's restaurants are, the rest anywhere.
 */
export function merchants(c: CompanySpec): MerchantSpec[] {
  const r = rng(c.seed * 104729);
  const doors = addresses();
  const busy = doors.filter((d) => /Boulevard|Avenue/.test(d.street) || Math.hypot(d.x - 3500, d.y - 3000) < 900);
  const out: MerchantSpec[] = [];
  const names = new Set<string>();
  for (let i = 1; i <= c.merchants; i++) {
    const door = r() < 0.75 ? pick(r, busy) : pick(r, doors);
    const kind = pick(r, KINDS);
    let n: string;
    do n = `The ${pick(r, ADJ)} ${pick(r, NOUN)}`;
    while (names.has(`${n}${kind}`));
    names.add(`${n}${kind}`);
    out.push({ code: `M${String(i).padStart(3, '0')}`, name: `${n} ${kind}`, kind, address: door.label, loc: door.point });
  }
  return out;
}

/**
 * How busy an hour of the day is, against the busiest: a lunch wave and a
 * bigger dinner one, a quiet night.
 */
export function wave(hour: number): number {
  const lunch = Math.exp(-((hour - 12.75) ** 2) / 1.2);
  const dinner = 1.25 * Math.exp(-((hour - 19.5) ** 2) / 2.2);
  const base = hour >= 8 && hour <= 23 ? 0.18 : 0.03;
  return Math.min(1, base + 0.8 * Math.max(lunch, dinner));
}
