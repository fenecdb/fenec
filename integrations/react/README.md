# @fenecdb/react

`useLiveQuery` for fenecdb: a query's rows, rendered again every time they
change -- over a database in the page alone, as the app's state, or over a
replica synced from a server.

```js
import { Fenec, persist, restore } from '@fenecdb/web';
import wasm from '@fenecdb/web/fenec.wasm?url';   // Vite; your bundler's asset import otherwise
import { FenecProvider, useFenec, useLiveQuery } from '@fenecdb/react';

const db = await Fenec.open(wasm);
if (!(await restore(db, 'todos'))) db.run('create collection todos (title text, done bool @hash)');

function Open() {
  const db = useFenec();
  const rows = useLiveQuery(db.from('todos').where('done', false).order('title'));
  if (rows === undefined) return <Spinner />;
  return rows.map((t) => (
    <li key={t.id} onClick={() => db.from('todos').where('id', t.id).update({ done: true })}>{t.title}</li>
  ));
}

<FenecProvider db={db}><Open /></FenecProvider>
```

The rows come from the database in the page, so no render waits on the
network: `live` runs the query again after every write to a collection it
reads, once for all the writes of a task, and the component renders what it
answers. A write from an event handler is all it takes; `persist(db,
'todos')` keeps the state between visits.

The same components work over a synced replica, which also hears the writes
other clients make through the server:

```js
import { sync } from '@fenecdb/web';

const db = await sync({
  url: 'http://127.0.0.1:8080',                    // fenec-server --http
  shapes: [{ collection: 'todos' }],
  wasm,
  collation: '/collate/',                          // @fenecdb/web's collate/, served as static files
});

<FenecProvider db={db}><Open /></FenecProvider>
```

`useLiveQuery` is `undefined` until the first answer, and a query built
again on every render does not subscribe again unless it asks something
else. A query built with `db.from(...)` brings its database along; a FenecQL
text runs on the provider's, and names what it reads:
`useLiveQuery('get todos count', { collections: ['todos'] })`. A query that
fails throws while rendering, to the nearest error boundary. A query that
asks for facets has them on its rows, counted over every match, so a
results list and its filter sidebar come from one hook:

```js
const rows = useLiveQuery(db.from('products').match('title', text).facet('brand').limit(20));
// rows.facets.brand: [{ value: 'acme', count: 12 }, ...]
```

React is the one peer dependency; the database is `@fenecdb/web`'s.
Full reference: https://fenecdb.com/docs/javascript#state and
https://fenecdb.com/docs/integrations
