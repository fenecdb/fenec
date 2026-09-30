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
  from,
  not,
  or,
  persist,
  raw,
  restore,
  sync,
  type Row,
  type Sparse,
  type Timestamp,
  type TypedFrom,
  type Vector,
} from '../fenec.js';

type Article = {
  title: string;
  body: string;
  year: number;
  tags: string[];
  published: Timestamp;
  embed: Vector;
  splade: Sparse | null;
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
  expect<{ title: string; year: number }[]>(titles);
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

  await db.from('articles').where('tags', 'has', 'rust').where('year', 'in', [2023, 2024]).count();
  await db.from('articles').where(or({ year: 2024 }, not({ title: 'x' }), and(raw('year > $1', 2020)))).first();
  await db.from('articles').order('title', 'asc', { collate: 'tr' }).offset(10).explain();

  const grouped = await db.from('articles').select('year', 'count(*)', 'avg(year)').group('year').rows();
  expect<number>(grouped[0].year);

  const withReviews = await db
    .from('articles')
    .lookup<'reviews', Review>('reviews', { on: 'product_id', where: { stars: 5 }, limit: 3 })
    .rows();
  expect<number>(withReviews[0].reviews[0].stars);

  await db.from('articles').insert([{ title: 'a', published: new Date(), embed: new Float32Array(3) }]);
  await db.from('articles').where('id', 1).update({ year: 2025 });
  await db.from('articles').where('year', '<', 2000).delete();
  // @ts-expect-error -- a year is a number
  db.from('articles').insert({ year: 'soon' });

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

  await persist(db, 'app');
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
}
