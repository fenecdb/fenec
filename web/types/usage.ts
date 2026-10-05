// What a caller writes against web/fenec.d.ts, type-checked under --strict
// (`make types-check`): each line is one the types must take, and each
// `@ts-expect-error` one they must refuse -- a declaration gone loose takes
// everything, and that is a drift too.

import {
  Fenec,
  FenecHttp,
  FenecError,
  and,
  connect,
  expr,
  from,
  inc,
  not,
  openFile,
  or,
  persist,
  raw,
  restore,
  sync,
  type Json,
  type Row,
  type Sparse,
  type Timestamp,
  type TypedFrom,
  type Vector,
} from '../fenec.js';
import * as client from '../client.js';

// As `fenec types` writes a collection: a field not `required` reads null.
type Article = {
  title: string;
  body: string | null;
  year: number | null;
  tags: string[] | null;
  published: Timestamp | null;
  embed: Vector | null;
  splade: Sparse | null;
  meta: Json | null;
};
type Review = { product_id: number; stars: number; text: string };
type Schema = { articles: Article; reviews: Review };

const expect = <T>(value: T): T => value;

export async function local(bytes: Uint8Array, module: WebAssembly.Module) {
  const db = await Fenec.open<Schema>(bytes);
  // A Worker hands the module compiled.
  await Fenec.open<Schema>(module);
  await Fenec.open('./fenec.wasm', { collation: (name: string) => fetch(`/collate/${name}.bin`) });

  const titles = await db.from('articles').select('title', 'year').where('year', '>=', 2024).rows();
  expect<{ title: string; year: number | null }[]>(titles);
  // @ts-expect-error -- no such field
  db.from('articles').select('nope');
  // @ts-expect-error -- no such collection
  db.from('nothing');

  const near = await db.from('articles').select('title').near('embed', new Float32Array(3), { ef: 64 }).limit(5).rows();
  expect<{ title: string; _score: number }[]>(near);
  await db.from('articles').near('splade', '{1:0.5}/30522').rows();
  // @ts-expect-error -- `near` takes a vector field
  db.from('articles').near('title', [1, 2, 3]);

  await db.from('articles').match('body', 'rust wasm').rerank('embed', [0.1, 0.2]).rows();
  await db.from('articles').match('body', 'rust').near('embed', [0.1]).fuse({ candidates: 40 }).rows();
  // @ts-expect-error -- `match` takes a text field
  db.from('articles').match('year', 'x');

  // Marks answer under their labels, typed by their tags; facets beside
  // the rows, typed by the field's values -- a list's one by one.
  const found = await db
    .from('articles')
    .select('title')
    .highlight('title')
    .highlight('body', { pre: '<mark>', post: '</mark>' })
    .snippet('body', 20, { ellipsis: '…' })
    .match('body', 'rust')
    .facet('year', { top: 5 })
    .facet('tags')
    .facet('meta.lang')
    .limit(10)
    .rows();
  expect<[number, number][] | null>(found[0]['highlight(title)']);
  expect<string | null>(found[0]['highlight(body)']);
  expect<string | undefined>(found[0]['snippet(body)']?.text);
  expect<number | null | undefined>(found.facets.year[0]?.value);
  expect<string | null | undefined>(found.facets.tags[0]?.value);
  expect<number>(found.facets['meta.lang'][0].count);
  // @ts-expect-error -- not asked for
  void found.facets.title;
  // A range facet's values are its ranges; a disjunctive one is typed as any.
  const banded = await db
    .from('articles')
    .where('year', 2024)
    .facet('year', { ranges: [2000, 2010, 2020], disjunctive: true })
    .facet('tags', { top: 3, disjunctive: true })
    .limit(0)
    .rows();
  expect<[number, number] | null>(banded.facets.year[0].value);
  expect<string | null>(banded.facets.tags[0].value);
  // @ts-expect-error -- ranges are numbers
  db.from('articles').facet('year', { ranges: ['a', 'b'] });
  // @ts-expect-error -- disjunctive is a boolean
  db.from('articles').facet('year', { disjunctive: 'yes' });
  // A query whose type names no facet reads them untyped, and maybe absent.
  // @ts-expect-error -- maybe absent
  void (await db.from('articles').rows()).facets.year;
  // Built in steps, a `let` keeps the type it was declared with: the facets
  // asked after are still there, each field's counts untyped.
  let stepped = db.from('articles').select('title');
  if (Math.random() > 0.5) stepped = stepped.where('year', 2024);
  stepped = stepped.facet('year');
  const steps = await stepped.limit(5).rows();
  expect<number | undefined>(steps.facets?.year?.[0]?.count);
  expect<string>(steps[0].title);
  // Asked in the declaration, each field's type is kept through the steps.
  let kept = db.from('articles').select('title').facet('year');
  if (Math.random() > 0.5) kept = kept.where('year', 2024).order('title');
  expect<number | null | undefined>((await kept.rows()).facets.year[0]?.value);
  // @ts-expect-error -- marks are of a text field
  db.from('articles').highlight('year');
  // @ts-expect-error -- both tags, or neither
  db.from('articles').highlight('body', { pre: '<b>' });
  db.live(db.from('articles').facet('year'), (rows) => expect<number>(rows.facets.year.length));

  await db.from('articles').where('tags', 'has', 'rust').where('year', 'in', [2023, 2024]).count();
  // `in` takes a query whose one column is the list: `in (get ...)`.
  await db.from('articles').where({ year: { in: db.from('reviews').select('stars') } }).count();
  // The time a read of a collection whose rows expire is answered at.
  db.now = () => 1_800_000_000_000;
  // @ts-expect-error -- the time is a number
  db.now = () => 'soon';
  // A path into a json field, where a field goes.
  const tr = await db
    .from('articles')
    .select('title', 'meta.source.site')
    .where('meta.lang', '=', 'tr')
    .where('meta.source.rank', { gte: 2 })
    .order('meta.source.rank', 'desc')
    .rows();
  expect<{ title: string; 'meta.source.site': Json }[]>(tr);
  await db.from('articles').where('meta.lang', 'in', ['tr', 'en']).count();
  await db.from('articles').insert({ title: 'j', meta: { lang: 'tr', source: { rank: 3 } } });
  // @ts-expect-error -- `title` is text, which no path reads into
  db.from('articles').where('title.x', 1);
  await db.from('articles').where(or({ year: 2024 }, not({ title: 'x' }), and(raw('year > $1', 2020)))).first();
  await db.from('articles').order('title', 'asc', { collate: 'tr' }).offset(10).explain();

  const grouped = await db.from('articles').select('year', 'count(*)', 'avg(year)').group('year').rows();
  expect<number | null>(grouped[0].year);

  const withReviews = await db
    .from('articles')
    .lookup<'reviews', Review>('reviews', { on: 'product_id', where: { stars: 5 }, limit: 3 })
    .rows();
  expect<number>(withReviews[0].reviews[0].stars);
  // Untyped, the child's fields are not taken from its `where` alone.
  await db
    .from('articles')
    .lookup('reviews', { on: 'product_id', where: { stars: { gte: 4 } }, order: [['created', 'desc']], limit: 3 })
    .rows();

  await db.from('articles').insert([{ title: 'a', published: new Date(), embed: new Float32Array(3) }]);
  await db.from('articles').where('id', 1).update({ year: 2025 });
  await db.from('articles').where('year', '<', 2000).delete();
  // @ts-expect-error -- a year is a number
  db.from('articles').insert({ year: 'soon' });
  // A value worked out over the row; a lock taken, or not, in one statement.
  expect<number>(await db.from('articles').where('id', 1).update({ year: inc(1), published: expr('now()') }));
  expect<number>(await db.from('articles').insert({ title: 'lock' }, { ifAbsent: true }));
  // @ts-expect-error -- inc takes a number
  inc('1');
  // @ts-expect-error -- ifAbsent is a boolean
  db.from('articles').toInsert({ title: 'x' }, { ifAbsent: 1 });
  // A write that must write one row, or its batch goes back.
  expect<number>(await db.from('articles').where('id', 1).update({ year: inc(-1) }, { require: 1 }));
  expect<number>(await db.from('articles').where('id', 1).delete({ require: 1 }));
  db.from('articles').toInsert({ title: 'x' }, { ifAbsent: true, require: 1 });
  db.from('articles').toUpdate({ year: 1 }, { all: true, require: 3 });
  // @ts-expect-error -- require is a count
  db.from('articles').where('id', 1).toDelete({ require: '1' });
  expect<number>((await db.from('articles').where('year', 2024).limit(1).require(1).rows()).length);
  // @ts-expect-error -- a read's require is a count too
  db.from('articles').require('1');

  const [sql, params] = db.from('articles').where('year', 2024).toFenecQL();
  expect<string>(sql);
  expect<unknown[]>(params);

  db.run('create collection t (n int)');
  expect<any[]>(db.rows('get t'));
  await db.query('get t order n collate und');
  await db.collation('greek', 'cyrillic');
  const image: Uint8Array = db.snapshot();
  db.journal();
  const { replace, bytes: drained } = db.drain();
  expect<boolean>(replace);
  expect<number>(db.load(image) + drained.length);
  expect<number>(db.changes(0).seq + db.changeSeq);

  // Live queries over the database in the page: a builder query's rows typed.
  const stop = db.live(db.from('articles').select('title').where('year', 2024), (rows) => expect<string>(rows[0].title));
  stop();
  db.live('get articles count', (rows) => rows.length, { collections: ['articles'], onError: (e) => e });
  db.live(['get reviews where stars > $1', [3]], () => {});
  db.onError = (e: unknown) => console.error(e);
  expect<string[] | null>(db.from('articles').reads);
  // @ts-expect-error -- the callback is handed the rows
  db.live(db.from('articles'), (rows: number) => rows);
  // @ts-expect-error -- a query, a text, or [text, params]
  db.live(42, () => {});

  await persist(db, 'app');
  // A file of the origin private file system, in a directory of the page's.
  const file = await openFile(db, 'app.fenec', { dir: await navigator.storage.getDirectory() });
  expect<Uint8Array>(file.bytes());
  // @ts-expect-error -- a directory is a handle, not its name
  openFile(db, 'app.fenec', { dir: 'data' });
  expect<boolean>(await restore(db, 'app'));
  db.close();
}

export async function remote() {
  const http = connect<Schema>('http://127.0.0.1:8080', { token: 't' });
  expect<FenecHttp<Schema>>(http);
  const rows: Row<Article>[] = await http.from('articles').rows();
  expect<string>(rows[0].title);
  await Fenec.connect<Schema>('http://127.0.0.1:8080').from('reviews').where('stars', 5).rows();

  const typed: TypedFrom<Schema> = from as TypedFrom<Schema>;
  typed('articles').where('year', '>=', 2024).toFenecQL();

  const replica = await sync<Schema>({ url: 'http://127.0.0.1:8080', shapes: [{ collection: 'articles' }] });
  await replica.from('articles').where('year', 2024).rows();

  try {
    await http.rows('get nothing');
  } catch (e) {
    if (e instanceof FenecError) expect<string>(e.message);
  }

  // Live queries over the server: a builder query knows what it reads, a
  // text names it.
  const stop = http.live(http.from('articles').where('year', 2024), (rows) => expect<string>(rows[0].title));
  stop();
  http.live('get articles', () => {}, { collections: ['articles'] });
  // @ts-expect-error -- a text alone does not say what it reads
  http.live('get articles', () => {});
  // Polled, it need not: the server says whether what it read was written.
  http.live('get articles', () => {}, { poll: 5000 });
  http.live(http.from('articles').where('year', 2024), (rows) => expect<string>(rows[0].title), { poll: 5000 });

  // `/batch` and keys: a query, a text, or [text, params] each.
  const articles = http.from('articles');
  const done = await http.batch(
    [articles.where('year', 2024).toUpdate({ year: 2025 }, { require: 1 }), articles.select('title'), 'get articles count'],
    { idempotencyKey: 'k' },
  );
  expect<boolean>(done.replayed);
  expect<number | null>(done.seq);
  const first = done.results[0];
  if ('affected' in first) expect<number>(first.affected);
  try {
    await http.batch(['get articles']);
  } catch (e) {
    if (e instanceof FenecError) expect<number | undefined>(e.at);
  }
  // @ts-expect-error -- a statement is a query, a text or [text, params]
  void http.batch([42]);
  await http.run('put articles {title: $1}', ['a'], { idempotencyKey: 'k2' });
  expect<FenecHttp<Schema>>(http.withIdempotencyKey('k3'));
  expect<number | null>(http.seq);

  // `@fenecdb/web/client`: the same classes and builder, no engine.
  const bare = client.connect<Schema>('http://127.0.0.1:8080');
  expect<FenecHttp<Schema>>(bare);
  expect<string>(client.from('articles').where('year', 2024).toFenecQL()[0]);
  // @ts-expect-error -- the client holds no engine
  void client.Fenec;
  // @ts-expect-error -- nor the sync layer
  void client.sync;
}
