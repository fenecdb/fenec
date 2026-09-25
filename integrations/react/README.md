# @fenecdb/react

`useLiveQuery` for fenecdb: a synced query's rows, rendered again every time
they change.

```js
import { sync } from '@fenecdb/web';
import wasm from '@fenecdb/web/fenec.wasm?url';   // Vite; your bundler's asset import otherwise
import { FenecProvider, useFenec, useLiveQuery } from '@fenecdb/react';

const db = await sync({
  url: 'http://127.0.0.1:8080',                    // fenec-pg --http
  shapes: [{ collection: 'tasks' }],
  wasm,
  collation: '/collate/',                          // @fenecdb/web's collate/, served as static files
});

function Open() {
  const rows = useLiveQuery(useFenec().from('tasks').where('done', false).order('title'));
  if (rows === undefined) return <Spinner />;
  return rows.map((t) => <Task key={t.id} {...t} />);
}

<FenecProvider db={db}><Open /></FenecProvider>
```

The rows come from the local replica, so no render waits on the network: the
sync layer runs the query again after every local change -- a write of this
tab's, or one the server streamed in -- and the component renders what it
answers. `useLiveQuery` is `undefined` until the first answer, and a query
built again on every render does not subscribe again unless it asks
something else. A query that fails throws while rendering, to the nearest
error boundary.

React is the one peer dependency; the synced database is `@fenecdb/web`'s.
Full reference: https://fenecdb.com/docs/integrations
