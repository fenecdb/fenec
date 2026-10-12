// Plover's database on one machine: fenec-server tenant nodes (`--dir`) and
// fenec-shard in front of them. `npm run cluster` runs it for development;
// each test file starts one of its own over a new directory.
//
// Every node takes the same flags: the secret JSON Web Tokens are signed
// with (HS256, from a file), the policy, CORS for the site's origin -- the
// dispatcher's live map talks to the router from the browser -- and room
// for the connections a fleet holds open: each courier waiting for an order
// is a request held at the node (`Fenec-Wait`), a connection and a thread
// as a subscription is, and 100, the default, is a fleet of 90.
import { spawn, type ChildProcess } from 'node:child_process';
import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { JWT_SECRET, POLICY_FILE } from './config';

const root = fileURLToPath(new URL('../../..', import.meta.url));

export function binary(name: 'fenec-server' | 'fenec-shard'): string {
  const envName = name === 'fenec-server' ? 'FENEC_SERVER' : 'FENEC_SHARD';
  const found = [process.env[envName], join(root, 'target/release', name), join(root, 'target/debug', name)].find(
    (b) => b && existsSync(b),
  );
  if (!found) throw new Error(`no ${name}: cargo build --release -p ${name}, or set ${envName}`);
  return found;
}

export interface ClusterOptions {
  dir: string;
  /**
   * The router listens here and the nodes on the ports after it: for a
   * cluster over a directory kept from one run to the next, whose router
   * the site's PLOVER_ROUTER names. Without it every process takes a port
   * the system picks (`127.0.0.1:0`) and says which on its `listening on:`
   * line, so a test's cluster never meets another's, or anything else, on
   * a port it took for free -- a fixed one was "Address already in use"
   * beside any other suite on the machine, or a server of someone else's
   * answering in its place.
   */
  basePort?: number;
  nodes?: number;
  cors?: string;
  operatorToken: string;
  shardToken: string;
  /** `--sync` of every node: `always` fsyncs a write before it is answered. */
  sync?: string;
  /** Extra flags for every node. */
  nodeFlags?: string[];
}

interface Proc {
  child: ChildProcess;
  out: string[];
  exited: Promise<void>;
}

export class Cluster {
  readonly nodeNames: string[];
  #procs = new Map<string, Proc>();
  /** Where each process said it listens, by name: `router`, `n1`, ... */
  #urls = new Map<string, string>();

  constructor(readonly opts: ClusterOptions) {
    this.nodeNames = Array.from({ length: opts.nodes ?? 2 }, (_, i) => `n${i + 1}`);
  }

  get routerUrl(): string {
    return this.#url('router');
  }

  nodeUrl(name: string): string {
    return this.#url(name);
  }

  #url(name: string): string {
    const url = this.#urls.get(name);
    if (!url) throw new Error(`${name} has not started`);
    return url;
  }

  /** The address `name` is to listen on: its own port past the base, or one the system picks. */
  #listen(name: string): string {
    const base = this.opts.basePort;
    return `127.0.0.1:${base === undefined ? 0 : name === 'router' ? base : base + 1 + this.nodeNames.indexOf(name)}`;
  }

  adminToken(name: string): string {
    return `plover-dev-admin-${name}`;
  }

  log(name: string): string {
    return this.#procs.get(name)?.out.join('') ?? '';
  }

  async start(): Promise<void> {
    mkdirSync(this.opts.dir, { recursive: true });
    writeFileSync(join(this.opts.dir, 'jwt.secret'), JWT_SECRET, { mode: 0o600 });
    for (const n of this.nodeNames) await this.startNode(n);
    await this.#run('router', binary('fenec-shard'), [
      '--listen',
      this.#listen('router'),
      '--directory',
      join(this.opts.dir, 'shard.fenec'),
      '--token',
      this.opts.shardToken,
      '--max-connections',
      '4000',
      // The page's streams go through the router: its own answers -- a
      // node it cannot reach, a tenant being moved -- carry the site's CORS
      // rule too, and its preflights are answered here.
      ...(this.opts.cors ? ['--http-cors', this.opts.cors] : []),
    ], {});
    for (const n of this.nodeNames) {
      const res = await fetch(`${this.routerUrl}/_shard/nodes/${n}`, {
        method: 'PUT',
        headers: { authorization: `Bearer ${this.opts.shardToken}` },
        // By the address the node said it listens on, which is the one the
        // system gave it.
        body: JSON.stringify({ addr: new URL(this.nodeUrl(n)).host, token: this.adminToken(n) }),
      });
      if (!res.ok) throw new Error(`register ${n}: ${res.status} ${await res.text()}`);
    }
  }

  async startNode(name: string): Promise<void> {
    const dir = join(this.opts.dir, name);
    mkdirSync(dir, { recursive: true });
    await this.#run(name, binary('fenec-server'), [
      '--dir',
      dir,
      '--http',
      this.#listen(name),
      '--admin-token',
      this.adminToken(name),
      '--jwt-secret-file',
      join(this.opts.dir, 'jwt.secret'),
      '--policy',
      POLICY_FILE,
      '--sync',
      this.opts.sync ?? '250',
      '--max-connections',
      '2000',
      // Every dispatcher's map holds a stream for positions and one for
      // orders, each live query one more, each customer following an order
      // one: 64, the default, is each tenant's, and every one of them is
      // also one of the node's --max-connections, which its tenants share --
      // each tenant's streams and held claims a quarter of it unless its
      // plan's quota says otherwise.
      '--http-max-streams',
      '2000',
      ...(this.opts.cors ? ['--http-cors', this.opts.cors] : []),
      ...(this.opts.nodeFlags ?? []),
    ], { FENEC_HTTP_TOKEN: this.opts.operatorToken });
  }

  /** SIGKILL: the node is gone, mid-write, as a machine lost would be. */
  async killNode(name: string): Promise<void> {
    const p = this.#procs.get(name);
    if (!p) return;
    p.child.kill('SIGKILL');
    await p.exited;
    this.#procs.delete(name);
  }

  async stop(): Promise<void> {
    for (const [, p] of this.#procs) p.child.kill('SIGTERM');
    await Promise.all([...this.#procs.values()].map((p) => p.exited));
    this.#procs.clear();
  }

  async #run(name: string, bin: string, args: string[], env: Record<string, string>): Promise<void> {
    const child = spawn(bin, args, { env: { ...process.env, ...env }, stdio: ['ignore', 'pipe', 'pipe'] });
    const out: string[] = [];
    const keep = (d: Buffer) => {
      out.push(d.toString());
      if (out.length > 2000) out.splice(0, 1000);
    };
    child.stdout!.on('data', keep);
    child.stderr!.on('data', keep);
    const exited = new Promise<void>((r) => child.on('exit', () => r()));
    // A test or the bench that dies leaves no node behind.
    process.on('exit', () => child.kill('SIGKILL'));
    this.#procs.set(name, { child, out, exited });
    // Up once it says where it listens: fenec-server's and fenec-shard's
    // `listening on: http://<address>  [...]` line names the port it bound,
    // the one the system picked for `:0` -- read to the space after it, so
    // a line that came in two pieces is not read short.
    const t0 = Date.now();
    for (;;) {
      const at = /listening on: (http:\/\/\S+)\s/.exec(out.join(''));
      if (at) {
        this.#urls.set(name, at[1]);
        return;
      }
      if (child.exitCode !== null) throw new Error(`${name} exited: ${out.join('')}`);
      if (Date.now() - t0 > 30_000) throw new Error(`${name} did not start: ${out.join('')}`);
      await new Promise((r) => setTimeout(r, 20));
    }
  }
}
