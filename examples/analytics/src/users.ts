// Who signs in to the dashboard. A password is kept as scrypt's output and
// its salt; a check takes the same time whether or not the name exists.
import { randomBytes, scryptSync, timingSafeEqual } from 'node:crypto';
import { CONTROL } from './config.ts';
import { db } from './db.ts';
import type { Site } from './setup.ts';

export interface User {
  name: string;
  display: string;
  sites: string[];
}

export function hashPassword(password: string): string {
  const salt = randomBytes(16);
  return `${salt.toString('base64url')}.${scryptSync(password, salt, 32).toString('base64url')}`;
}

const DUMMY = hashPassword('not a password anyone has');

export async function signIn(name: string, password: string): Promise<User | null> {
  const [u] = await db(CONTROL, 'app').rows('get users where name = $1 limit 1', [name]);
  const [salt, want] = ((u?.hash as string | undefined) ?? DUMMY).split('.');
  const got = scryptSync(password, Buffer.from(salt, 'base64url'), 32);
  const ok = timingSafeEqual(got, Buffer.from(want, 'base64url'));
  return u && ok ? { name: u.name, display: u.display, sites: u.sites } : null;
}

export async function user(name: string): Promise<User | null> {
  const [u] = await db(CONTROL, 'app').rows('get users select name, display, sites where name = $1 limit 1', [name]);
  return u ? (u as User) : null;
}

/** The sites, by their tracker's key and by name, read again every 30 s. */
export class Sites {
  #byKey = new Map<string, Site>();
  #byName = new Map<string, Site>();
  #loaded = 0;

  #loading: Promise<void> | null = null;
  /** Keys and names looked for and not found, and when: each is looked for again after 5 s. */
  #missed = new Map<string, { at: number; read: Promise<void> }>();

  /** Read again when 30 s old, or `now` when asked by a miss; concurrent callers share one read. */
  #load(now = false): Promise<void> {
    if (!now && Date.now() - this.#loaded < 30_000) return Promise.resolve();
    this.#loading ??= (async () => {
      try {
        const rows = (await db(CONTROL, 'app').rows('get sites limit 10000')) as Site[];
        this.#byKey = new Map(rows.map((s) => [s.key, s]));
        this.#byName = new Map(rows.map((s) => [s.name, s]));
        this.#loaded = Date.now();
      } finally {
        this.#loading = null;
      }
    })();
    return this.#loading;
  }

  /**
   * A key or name not known has the sites read again -- a site just added
   * -- once, then not for 5 s: random keys sent to the endpoint cost one
   * read each at most, one at a time.
   */
  async #miss(k: string) {
    const m = this.#missed.get(k);
    if (m && Date.now() - m.at < 5000) return m.read;
    if (this.#missed.size > 10_000) this.#missed.clear();
    const read = (async () => {
      await this.#loading;
      await this.#load(true);
    })();
    this.#missed.set(k, { at: Date.now(), read });
    await read;
  }

  async byKey(key: string): Promise<Site | undefined> {
    await this.#load();
    if (!this.#byKey.has(key)) await this.#miss(`k:${key}`);
    return this.#byKey.get(key);
  }

  async byName(name: string): Promise<Site | undefined> {
    await this.#load();
    if (!this.#byName.has(name)) await this.#miss(`n:${name}`);
    return this.#byName.get(name);
  }

  async all(): Promise<Site[]> {
    await this.#load();
    return [...this.#byName.values()];
  }

  forget() {
    this.#loaded = 0;
  }
}
