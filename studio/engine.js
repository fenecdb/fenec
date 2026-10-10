// A database in the page, answering as a server answers fenec studio.
//
// The studio reaches its database through one surface: the statements
// (`run`, `batch`), a shape's rows as they change (`subscribe`) and the
// requests its views send whole (`/query`, `/batch`, `/_schema`,
// `/_schema/plan`). Over HTTP that surface is the server's (connect.js);
// here it is a `Fenec` -- the browser module, in the worker local.js starts
// -- and every answer is put in the shape the server gives it, so a view
// never asks which it has: the same rows, the status the server would
// answer a refusal with, `Fenec-Seq` where a write sets it. What only a
// server has -- a token's identity, its metrics, its statements' counts,
// a router's tenants -- is not here, and the studio leaves those views out
// rather than fake them: a request for one is a 404.
//
// No DOM and no worker here, so `studio/test/transport.test.mjs` holds
// every answer to a real fenec-server's for the same statements.

/** An engine error's kind, by the words its message starts with, as `api::status_of` maps it. */
const STATUS = [
  ['not found: ', 404],
  ['already exists: ', 409],
  ['duplicate: ', 409],
  ['unmet: ', 412],
  ['type error: ', 400],
  ['query error: ', 400],
  ['read only: ', 403],
  ['denied: ', 403],
];

/**
 * `[status, message]` of an engine error, as `/query` writes it: the
 * message without its kind -- but for a statement that does not parse,
 * which a server refuses whole, kind and all, before it runs anything.
 * The module answers both alike; what a parse refuses names the position
 * it stopped at.
 */
export function statusOf(message) {
  if (PARSE.test(message)) return [400, message];
  for (const [prefix, status] of STATUS) {
    if (message.startsWith(prefix)) return [status, message.slice(prefix.length)];
  }
  const i = message.indexOf(': ');
  return [500, i > 0 ? message.slice(i + 2) : message];
}

/** What the lexer and the parser refuse a text with. */
const PARSE = /^query error: (position \d+:|parameters start at \$1)/;

/** An index of a description (`GET /_schema`) as FenecQL declares it. */
function indexText(ix) {
  const { kind, ...o } = ix;
  if (kind === 'ttl') {
    const unit = [['d', 86_400_000], ['h', 3_600_000], ['m', 60_000], ['s', 1000], ['ms', 1]].find(([, n]) => o.ms % n === 0);
    return `@ttl(${o.ms / unit[1]}${unit[0]})`;
  }
  const args = [];
  if (kind === 'hnsw') args.push(o.metric ?? 'cosine');
  for (const k of ['k1', 'b', 'm', 'ef_construction', 'ef_search', 'prefix', 'prefix_min', 'quant']) if (o[k] !== undefined) args.push(`${k}=${o[k]}`);
  if (o.chars) args.push('chars');
  return `@${kind}${args.length ? `(${args.join(', ')})` : ''}`;
}

/**
 * A description's collections as the FenecQL that makes them: the browser
 * module reads a description so -- reading its JSON was 8 KB of the module
 * -- and the studio's schema view sends JSON, as a server takes it.
 */
export function fenecqlOf(collections) {
  const out = [];
  for (const c of collections) {
    const fields = c.fields.map((f) =>
      [f.name, f.type, f.required && 'required', f.collate && `collate ${f.collate}`, f.index && indexText(f.index)].filter(Boolean).join(' '),
    );
    out.push(`create collection ${c.name} (${fields.join(', ')})`);
    for (const f of c.fields) for (const p of f.paths ?? []) out.push(`create index on ${c.name} (${f.name}.${p.path}) ${indexText(p.index)}`);
  }
  return out.join('\n');
}

/**
 * The module's `collections` answer as a server's (`schemas_json`): an
 * index `null` where there is none, a graph's `ef_search` written, and
 * `required` and `collate` from the description, which the module's
 * answer leaves out.
 */
function collections(list, described) {
  const byName = new Map((described?.collections ?? []).map((c) => [c.name, c]));
  return list.map((c) => {
    const d = byName.get(c.name);
    return {
      name: c.name,
      fields: c.fields.map((f) => {
        const df = d?.fields.find((x) => x.name === f.name);
        let index = f.index === 'none' ? null : f.index;
        if (df?.index?.kind === 'hnsw' && index) index = index.replace(/^(hnsw\([^,]+, m=\d+)/, `$1, ef_search=${df.index.ef_search}`);
        const out = { name: f.name, type: f.type, index, required: !!df?.required };
        if (df?.collate) out.collate = df.collate;
        return out;
      }),
    };
  });
}

export class Engine {
  #db;
  #subs = new Map();

  /** `db` is an open `Fenec`. */
  constructor(db) {
    this.#db = db;
  }

  get db() {
    return this.#db;
  }

  /**
   * The database replaced -- reset, or brought back from this browser --
   * and every subscription seeded again over the new one, as a server's
   * stream seeds again when it is opened again.
   */
  replace(db) {
    this.#db = db;
    for (const [id, s] of this.#subs) {
      s.stop();
      this.#follow(id, s.collection, s.shape, s.post);
    }
  }

  /**
   * One request as the server answers it: `{status, json, seq}`, `json`
   * the body. `wrote` says whether it may have changed the database, for
   * the page that keeps it.
   */
  async request(method, path, body) {
    const [route, query = ''] = path.split('?');
    try {
      if (method === 'POST' && route === '/query') {
        const { query: text, params = [] } = JSON.parse(body);
        return await this.#query(text, params);
      }
      if (method === 'POST' && route === '/batch') {
        const items = String(body)
          .split('\n')
          .filter((l) => l.trim())
          .map((l) => {
            const { query: text, params = [] } = JSON.parse(l);
            return [text, params];
          });
        return await this.#batch(items);
      }
      if (method === 'GET' && route === '/_schema') {
        return { status: 200, json: this.#db.describe(new URLSearchParams(query).get('as') === 'fenecql' ? 'fenecql' : 'json'), seq: null };
      }
      if (method === 'POST' && route === '/_schema/plan') {
        let d = null;
        try {
          d = JSON.parse(body);
        } catch {
          /* refused by the module in the server's words */
        }
        const asked = Array.isArray(d?.collections) ? { format: d.format, fenecql: fenecqlOf(d.collections), migrations: d.migrations } : body;
        return { status: 200, json: this.#db.checkSchema(asked, 'plan'), seq: null };
      }
    } catch (e) {
      const [status, error] = statusOf(e.message ?? String(e));
      return { status, json: { error }, seq: null };
    }
    return { status: 404, json: { error: `${method} ${route} is a server's: fenec studio over a database in the page answers /query, /batch and /_schema` }, seq: null };
  }

  /** `POST /query`: rows as an array, or with `facets` beside them; a write's count; a message. */
  async #query(text, params) {
    let r;
    try {
      // `query`, which fetches the collation data a text of another script
      // needs and runs it again, where `run` refuses it.
      r = await this.#db.query(text, params);
    } catch (e) {
      const [status, error] = statusOf(e.message);
      return { status, json: { error }, seq: null, wrote: true };
    }
    if (Array.isArray(r.rows)) return { status: 200, json: r.facets ? { rows: r.rows, facets: r.facets } : r.rows, seq: null };
    if (r.kind === 'schemas') return { status: 200, json: collections(r.collections, this.#db.describe()), seq: null };
    const seq = this.#db.changeSeq;
    if (r.kind === 'affected') return { status: 200, json: { affected: r.count }, seq, wrote: true };
    return { status: 200, json: { message: r.message }, seq, wrote: true };
  }

  /** `POST /batch`: one block, `{ok, results}`; a refusal names its statement (`at`). */
  async #batch(items) {
    try {
      const got = await this.#db.batch(items);
      const results = got.results.map((x) => (x.collections ? { collections: collections(x.collections, this.#db.describe()) } : x));
      return { status: 200, json: { ok: results.length, results }, seq: got.seq, wrote: true };
    } catch (e) {
      // A statement that does not parse refuses the batch before it runs,
      // as a server names it: by its line.
      if (PARSE.test(e.message)) return { status: 400, json: { error: `batch line ${(e.at ?? 0) + 1}: ${e.message}` }, seq: null };
      const [status] = statusOf(e.message);
      return { status, json: { error: e.message, completed: e.completed ?? 0, at: e.at ?? 0 }, seq: null, wrote: true };
    }
  }

  /**
   * A shape's rows, then each change to them, as a server's subscription
   * sends them (`GET /<c>/changes`): `{type: 'seed', seq, rows}`, then
   * `{type: 'change', seq, puts, dels, schema}`. The page's live query
   * (`Fenec.live`) runs the shape again after every write to the
   * collection -- the change ring says which -- and what differs from the
   * rows it held is the change: a row new or written is put, one gone or
   * no longer in the shape deleted. A server also names as deleted a row
   * written outside the shape that it never sent, which a view passes over;
   * here only rows the shape held are.
   */
  subscribe(id, collection, shape, post) {
    this.#follow(id, collection, shape, post);
  }

  unsubscribe(id) {
    this.#subs.get(id)?.stop();
    this.#subs.delete(id);
  }

  #follow(id, collection, shape, post) {
    const where = shape?.where ? ` where (${shape.where})` : '';
    let held = null;
    let fields = null;
    const stop = this.#db.live(
      `get ${collection}${where}`,
      (rows) => {
        const seq = this.#db.changeSeq;
        const now = new Map(rows.map((r) => [r.id, JSON.stringify(r)]));
        const names = rows.length ? Object.keys(rows[0]).join() : fields;
        if (held === null) {
          held = now;
          fields = names;
          post({ type: 'seed', seq, rows });
          return;
        }
        const puts = rows.filter((r) => held.get(r.id) !== now.get(r.id));
        const dels = [...held.keys()].filter((k) => !now.has(k));
        const schema = names !== null && fields !== null && names !== fields;
        held = now;
        fields = names ?? fields;
        if (puts.length || dels.length || schema) post({ type: 'change', seq, puts, dels, schema });
      },
      { collections: [collection], onError: (e) => post({ type: 'error', message: e.message, status: statusOf(e.message)[0] }) },
    );
    this.#subs.set(id, { collection, shape, post, stop });
  }
}
