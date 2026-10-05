// The journal streamed to a sink outside the database -- here a file of
// JSON lines, standing in for a warehouse or a message bus.
//
// fenec-server keeps the consumer's place (`/_changes/consumers/<name>`),
// and a consumer moves it only after the lines it read are in the sink and
// synced. So each write reaches the sink at least once: a crash between the
// sink's sync and the commit hands the same writes over again. The entry's
// id (`entry`, @unique in the journal) is the deduplication key: the sink
// holds each id once, whatever came twice.
//
// The stream needs the operator's token: a scoped token is refused, since
// its filter could not hold back the deletion of a row it never saw.
import { closeSync, existsSync, fsyncSync, openSync, readFileSync, renameSync, writeFileSync, writeSync } from 'node:fs';

export interface SinkLine {
  entry: string;
  tx: string;
  account: string;
  currency: string;
  amount: number;
  kind: string;
  at: string;
  /** The change that wrote it. */
  seq: number;
}

export interface SinkOptions {
  /** fenec-server's base, `http://host:port/t/<tenant>` on a tenant node. */
  base: string;
  /** The operator's token. */
  token: string;
  file: string;
  name?: string;
  /** How long a read with nothing in it waits for a write, ms. */
  wait?: number;
  limit?: number;
  /** Commit at most this often, ms (default 250); 0 commits every page that held writes. */
  commitEvery?: number;
  /** Write where the consumer stands beside the file (default: yes). */
  status?: boolean;
  /** Called with the lines each write to the file added, once they are synced. */
  onWrite?: (lines: SinkLine[]) => void;
}

export class Sink {
  readonly name: string;
  #seen = new Set<string>();
  #fd: number;
  #stopped = false;
  #pending: SinkLine[] = [];
  #uncommitted = false;
  #committed = 0;
  #reported = -Infinity;
  /** Lines read twice and dropped: the at-least-once delivery, seen. */
  duplicates = 0;
  written = 0;
  /** The last change committed: where a restart begins. */
  since = 0;
  /** The last change read: where the next read begins. */
  cursor = 0;

  constructor(readonly opts: SinkOptions) {
    this.name = opts.name ?? 'journal-sink';
    // What the file holds already is what was delivered before a restart.
    if (existsSync(opts.file)) {
      for (const line of readFileSync(opts.file, 'utf8').split('\n')) {
        if (!line) continue;
        try {
          this.#seen.add((JSON.parse(line) as SinkLine).entry);
        } catch {
          // A last line a crash cut short: its entry will come again.
        }
      }
    }
    this.#fd = openSync(opts.file, 'a');
  }

  get size(): number {
    return this.#seen.size;
  }

  async #req(path: string, init: RequestInit = {}): Promise<Response> {
    return fetch(`${this.opts.base}${path}`, {
      ...init,
      headers: { authorization: `Bearer ${this.opts.token}`, ...(init.headers ?? {}) },
    });
  }

  /** Makes the consumer at the first write if it is new; where it stands otherwise. */
  async open(): Promise<number> {
    const res = await this.#req(`/_changes/consumers/${this.name}`, { method: 'POST', body: JSON.stringify({ since: 0 }) });
    if (res.status === 409 || res.ok) {
      const list = (await (await this.#req('/_changes/consumers')).json()) as { name: string; since: number }[];
      const me = (Array.isArray(list) ? list : []).find((c) => c.name === this.name);
      this.since = me?.since ?? 0;
      this.cursor = this.since;
      // A consumer that moved on while the file is empty lost its file (a
      // new disk, a sink moved): copy the journal again before streaming.
      if (this.since > 0 && this.size === 0) await this.backfill();
      return this.since;
    }
    throw new Error(`consumer ${this.name}: ${res.status} ${await res.text()}`);
  }

  /**
   * Reads one page from where the sink has read to, writes the journal's
   * entries it has not got and syncs the file, then commits. `crashBefore`
   * stops after the sync and before the commit, as a crash would.
   *
   * It reads by its own cursor (`since=`), not `?consumer=`, and commits
   * only a page that held writes: a commit is itself a write (a row of
   * `_consumers`), which the stream leaves out but counts, so a read from
   * the consumer's place answered at once past the commit before it, and a
   * sink committing every answer committed in a loop -- 30 000 writes in a
   * few idle minutes, each fsynced (README, "Gaps").
   */
  async step(opts: { crashBefore?: 'commit' } = {}): Promise<number> {
    const q = `?since=${this.cursor}&limit=${this.opts.limit ?? 1000}&wait=${this.opts.wait ?? 1000}`;
    const res = await this.#req(`/_changes${q}`);
    if (res.status === 410) {
      // The server no longer keeps the writes the consumer stands at (it
      // restarted, or the consumer fell further behind than the feed
      // holds): copy the journal as it stands, and go on from there.
      await this.backfill();
      return 0;
    }
    if (!res.ok) throw new Error(`/_changes: ${res.status} ${await res.text()}`);
    const next = Number(res.headers.get('fenec-next') ?? this.cursor);
    let n = 0;
    let writes = 0;
    let out = '';
    for (const line of (await res.text()).split('\n')) {
      if (!line) continue;
      writes++;
      const c = JSON.parse(line) as { seq: number; collection: string; op: string; doc?: Record<string, unknown> };
      if (c.collection !== 'journal' || c.op !== 'put' || !c.doc) continue;
      out += this.#line(c.doc, c.seq);
      n++;
    }
    this.#append(out);
    if (opts.crashBefore === 'commit') return n;
    this.cursor = Math.max(this.cursor, next);
    // A commit is a durable write of its own, a few ms under `--sync
    // always`: made for every page, it stood between each read and the
    // next, and an entry reached the file 13 ms after its transfer's answer
    // at the median. Made at most every `commitEvery` ms, a crash hands that
    // much over again, which the deduplication absorbs.
    this.#uncommitted ||= writes > 0;
    const due = performance.now() - this.#committed >= (this.opts.commitEvery ?? 250);
    if (this.#uncommitted && due && next > this.since) {
      const r = await this.#req(`/_changes/consumers/${this.name}`, { method: 'POST', body: JSON.stringify({ since: next }) });
      if (!r.ok) throw new Error(`commit ${next}: ${r.status} ${await r.text()}`);
      this.since = next;
      this.#uncommitted = false;
      this.#committed = performance.now();
    }
    if (this.opts.status !== false && performance.now() - this.#reported >= 1000) {
      this.#reported = performance.now();
      await this.#status();
    }
    return n;
  }

  /**
   * Where the consumer stands, beside the sink's file, for the console,
   * which holds no operator token to ask the server itself.
   */
  async #status() {
    const res = await this.#req('/_changes/consumers');
    if (!res.ok) return;
    const me = ((await res.json()) as { name: string; since: number; behind: number }[]).find((c) => c.name === this.name);
    const status: SinkStatus = {
      since: this.since,
      behind: me?.behind ?? 0,
      written: this.written,
      duplicates: this.duplicates,
      checked: Date.now(),
    };
    const tmp = `${this.opts.file}.status.tmp`;
    writeFileSync(tmp, JSON.stringify(status));
    renameSync(tmp, `${this.opts.file}.status`);
  }

  /** The journal read whole, in one snapshot, and the consumer moved to it. */
  async backfill(): Promise<void> {
    const body = JSON.stringify({ query: 'get journal' });
    const res = await this.#req('/batch', { method: 'POST', body });
    if (!res.ok) throw new Error(`backfill: ${res.status} ${await res.text()}`);
    const seq = Number(res.headers.get('fenec-seq'));
    const json = (await res.json()) as { results: { rows: Record<string, unknown>[] }[] };
    let out = '';
    for (const doc of json.results[0].rows) out += this.#line(doc, 0);
    this.#append(out);
    const r = await this.#req(`/_changes/consumers/${this.name}`, { method: 'POST', body: JSON.stringify({ since: seq }) });
    if (!r.ok) throw new Error(`commit ${seq}: ${r.status} ${await r.text()}`);
    this.since = seq;
    this.cursor = seq;
  }

  #line(doc: Record<string, unknown>, seq: number): string {
    const entry = doc.entry as string;
    if (this.#seen.has(entry)) {
      this.duplicates++;
      return '';
    }
    this.#seen.add(entry);
    this.written++;
    const line: SinkLine = {
      entry,
      tx: doc.tx as string,
      account: doc.account as string,
      currency: doc.currency as string,
      amount: Number(doc.amount),
      kind: doc.kind as string,
      at: doc.at as string,
      seq,
    };
    this.#pending.push(line);
    return JSON.stringify(line) + '\n';
  }

  #append(text: string) {
    if (!text) return;
    writeSync(this.#fd, text);
    fsyncSync(this.#fd);
    const lines = this.#pending;
    this.#pending = [];
    this.opts.onWrite?.(lines);
  }

  /** Runs until `stop()`. */
  async run(onError: (e: unknown) => void = console.error): Promise<void> {
    // The server may be starting, or the tenant not made yet: try again.
    for (;;) {
      try {
        await this.open();
        break;
      } catch (e) {
        onError(e);
        if (this.#stopped) return;
        await new Promise((r) => setTimeout(r, 1000));
      }
    }
    while (!this.#stopped) {
      try {
        await this.step();
      } catch (e) {
        onError(e);
        await new Promise((r) => setTimeout(r, 500));
      }
    }
  }

  stop() {
    this.#stopped = true;
  }

  close() {
    this.stop();
    closeSync(this.#fd);
  }
}

export interface SinkStatus {
  since: number;
  behind: number;
  written: number;
  duplicates: number;
  checked: number;
}

export function sinkStatus(file: string): SinkStatus | null {
  try {
    return JSON.parse(readFileSync(`${file}.status`, 'utf8')) as SinkStatus;
  } catch {
    return null;
  }
}

/** The sink's file, read back: entry id to amount, and how many lines. */
export function readSink(file: string): { entries: Map<string, SinkLine>; lines: number } {
  const entries = new Map<string, SinkLine>();
  let lines = 0;
  if (!existsSync(file)) return { entries, lines };
  for (const line of readFileSync(file, 'utf8').split('\n')) {
    if (!line) continue;
    lines++;
    const l = JSON.parse(line) as SinkLine;
    entries.set(l.entry, l);
  }
  return { entries, lines };
}
