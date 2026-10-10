// Invented traffic for the demo, the tests and the measurements: visitors
// who come back less each week, more on weekdays and in their own daytime,
// reading a product site's pages, some of them starting a signup and fewer
// finishing it. Seeded, so a run makes the same events every time.

export function rng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export function pick<T>(r: () => number, items: readonly (readonly [T, number])[]): T {
  let total = 0;
  for (const [, w] of items) total += w;
  let x = r() * total;
  for (const [v, w] of items) {
    x -= w;
    if (x < 0) return v;
  }
  return items[items.length - 1][0];
}

export interface SimEvent {
  eid: string;
  name: string;
  user: string;
  path: string;
  ref: string;
  country: string;
  device: string;
  browser: string;
  at: number;
  props: Record<string, string | number | boolean> | null;
}

// Country, its weight, and its UTC offset in hours (for when its visitors are awake).
const COUNTRIES = [
  ['US', 30, -6],
  ['DE', 11, 1],
  ['GB', 9, 0],
  ['IN', 8, 5],
  ['TR', 6, 3],
  ['FR', 6, 1],
  ['BR', 5, -3],
  ['CA', 5, -5],
  ['NL', 4, 1],
  ['JP', 4, 9],
  ['ES', 3, 1],
  ['PL', 3, 1],
  ['AU', 3, 10],
  ['SE', 2, 1],
  ['MX', 1, -6],
] as const;

const REFS = [
  ['', 34],
  ['google.com', 26],
  ['news.ycombinator.com', 9],
  ['github.com', 8],
  ['duckduckgo.com', 5],
  ['reddit.com', 5],
  ['t.co', 4],
  ['bing.com', 3],
  ['linkedin.com', 2],
  ['lobste.rs', 2],
  ['buttondown.email', 2],
] as const;

const PAGES = [
  ['/', 30],
  ['/pricing', 12],
  ['/docs/getting-started', 9],
  ['/blog/offline-first-notes', 8],
  ['/docs/sync', 6],
  ['/blog/why-we-left-the-cloud', 6],
  ['/changelog', 5],
  ['/blog/plain-text-forever', 5],
  ['/docs/shortcuts', 4],
  ['/about', 3],
  ['/blog/search-in-40-ms', 3],
  ['/docs/import', 3],
  ['/security', 2],
  ['/careers', 1],
] as const;

const DEVICES = [
  ['desktop', 58],
  ['mobile', 38],
  ['tablet', 4],
] as const;

const BROWSERS: Record<string, readonly (readonly [string, number])[]> = {
  desktop: [
    ['Chrome', 55],
    ['Safari', 17],
    ['Firefox', 12],
    ['Edge', 13],
    ['Opera', 3],
  ],
  mobile: [
    ['Safari', 46],
    ['Chrome', 44],
    ['Samsung', 7],
    ['Firefox', 3],
  ],
  tablet: [
    ['Safari', 70],
    ['Chrome', 30],
  ],
};

interface Visitor {
  id: string;
  country: string;
  offset: number;
  device: string;
  browser: string;
  firstDay: number;
  signedUp: boolean;
}

export interface SiteProfile {
  /** New visitors a day at the start. */
  daily: number;
  /** Growth a day, as a fraction. */
  growth: number;
  seed: number;
  /** Prefix of visitor ids, so two sites' visitors differ. */
  prefix: string;
}

/**
 * A site's visitors and their events, day by day. `day(ms)` makes one UTC
 * day's events, in time order.
 */
export class Traffic {
  // A field and an assignment rather than a parameter property: Node runs
  // this file with its types stripped -- monitoring/seed/seed.mjs imports it
  // with no build step -- and stripping takes only syntax it can erase.
  readonly profile: SiteProfile;
  readonly #r: () => number;
  readonly #visitors: Visitor[] = [];
  #n = 0;
  #e = 0;

  constructor(profile: SiteProfile) {
    this.profile = profile;
    this.#r = rng(profile.seed);
  }

  #visitor(day: number): Visitor {
    const r = this.#r;
    const c = pick(
      r,
      COUNTRIES.map(([code, w, off]) => [[code, off] as const, w] as const),
    );
    const device = pick(r, DEVICES);
    const v: Visitor = {
      id: `${this.profile.prefix}${(++this.#n).toString(36).padStart(7, '0')}`,
      country: c[0],
      offset: c[1],
      device,
      browser: pick(r, BROWSERS[device]),
      firstDay: day,
      signedUp: false,
    };
    this.#visitors.push(v);
    return v;
  }

  /** One UTC day's events, beginning at `day`, sorted by time. */
  day(day: number, index: number): SimEvent[] {
    const r = this.#r;
    const p = this.profile;
    const weekday = new Date(day).getUTCDay();
    const week = weekday === 0 || weekday === 6 ? 0.7 : 1;
    const fresh = Math.round(p.daily * week * (1 + p.growth) ** index * (0.85 + 0.3 * r()));
    const who: Visitor[] = [];
    for (let i = 0; i < fresh; i++) who.push(this.#visitor(day));
    // Returning visitors: fewer the longer ago they came first.
    for (const v of this.#visitors) {
      if (v.firstDay >= day) continue;
      const weeks = (day - v.firstDay) / (7 * 86_400_000);
      const chance = (v.signedUp ? 0.3 : 0.08) / (1 + weeks * 0.9);
      if (r() < chance * week) who.push(v);
    }
    const out: SimEvent[] = [];
    for (const v of who) out.push(...this.session(v, day));
    out.sort((a, b) => a.at - b.at);
    return out;
  }

  /** A visit: a few pages, from a referrer, perhaps a signup started and finished. */
  session(v: Visitor, day: number): SimEvent[] {
    const r = this.#r;
    // Awake from 7 to 23 local time, most in the afternoon and evening.
    const local = 7 + 16 * Math.sqrt(r());
    let at = day + ((((local - v.offset) % 24) + 24) % 24) * 3_600_000 + Math.floor(r() * 3_600_000) % 600_000;
    const ref = v.firstDay === day ? pick(r, REFS) : r() < 0.7 ? '' : pick(r, REFS);
    const pages = 1 + Math.floor(-Math.log(1 - r()) * 2.2);
    const out: SimEvent[] = [];
    const ev = (name: string, path: string, props: SimEvent['props'] = null) => {
      out.push({
        eid: `${this.profile.prefix}e${(++this.#e).toString(36)}`,
        name,
        user: v.id,
        path,
        ref: out.length === 0 ? ref : '',
        country: v.country,
        device: v.device,
        browser: v.browser,
        at,
        props,
      });
      at += 5_000 + Math.floor(r() * 90_000);
    };
    let sawPricing = false;
    for (let i = 0; i < pages; i++) {
      const path = i === 0 && ref === 'google.com' && r() < 0.5 ? pick(r, PAGES.slice(2)) : pick(r, PAGES);
      if (path === '/pricing') sawPricing = true;
      ev('pageview', path);
    }
    if (!v.signedUp && r() < (sawPricing ? 0.22 : 0.05)) {
      ev('signup_started', '/signup', { plan: r() < 0.7 ? 'free' : 'pro' });
      if (r() < 0.58) {
        ev('signup_completed', '/welcome', { plan: r() < 0.75 ? 'free' : 'pro' });
        v.signedUp = true;
      }
    }
    if (v.signedUp && r() < 0.1) ev('note_created', '/app');
    return out;
  }
}

export const DAY_MS = 86_400_000;

export function userAgent(device: string, browser: string): string {
  const os = device === 'desktop' ? 'Macintosh; Intel Mac OS X 10_15_7' : device === 'tablet' ? 'iPad; CPU OS 18_0 like Mac OS X' : 'iPhone; CPU iPhone OS 18_0 like Mac OS X';
  const mobile = device === 'mobile' ? ' Mobile/15E148' : '';
  switch (browser) {
    case 'Firefox':
      return `Mozilla/5.0 (${os}; rv:131.0) Gecko/20100101 Firefox/131.0`;
    case 'Edge':
      return `Mozilla/5.0 (${os}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0 Safari/537.36 Edg/129.0`;
    case 'Opera':
      return `Mozilla/5.0 (${os}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0 Safari/537.36 OPR/114.0`;
    case 'Samsung':
      return `Mozilla/5.0 (Linux; Android 14; SM-S921B) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/26.0 Chrome/122.0 Mobile Safari/537.36`;
    case 'Chrome':
      return device === 'desktop'
        ? `Mozilla/5.0 (${os}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0 Safari/537.36`
        : `Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0${device === 'mobile' ? ' Mobile' : ''} Safari/537.36`;
    default:
      return `Mozilla/5.0 (${os}) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0${mobile} Safari/604.1`;
  }
}
