// useLiveQuery for fenecdb: a query's rows, rendered again every time they
// change -- over a database in the page alone, or a replica synced from a
// server.
//
//   const db = await Fenec.open(wasm);            // or: await sync({ url, shapes })
//   <FenecProvider db={db}><Tasks /></FenecProvider>
//
//   function Tasks() {
//     const rows = useLiveQuery(useFenec().from('tasks').where('done', false).order('at'));
//     if (rows === undefined) return <Spinner />;
//     return rows.map((t) => <Task key={t.id} {...t} />);
//   }
//
// The rows come from the database in the page, so no render waits on the
// network: `live` runs the query again after every write it may read -- a
// write of this page's, or one a server streamed into a replica -- and the
// component renders with what it answers.

import { createContext, createElement, useContext, useEffect, useState } from 'react';

const FenecContext = createContext(null);

/** Makes `db`, a `Fenec` or a `FenecSync`, the one `useFenec` and `useLiveQuery` use below. */
export function FenecProvider({ db, children }) {
  return createElement(FenecContext.Provider, { value: db }, children);
}

/** The database of the nearest `FenecProvider`. */
export function useFenec() {
  const db = useContext(FenecContext);
  if (!db) throw new Error('useFenec: no FenecProvider above this component');
  return db;
}

/**
 * The rows of `query`, answered by the database in the page and given again
 * every time they change; `undefined` until the first answer. A query built
 * from `db.from(...)` brings its database along; one that does not -- or a
 * FenecQL text, or `[text, params]` -- is run on the provider's. A text's
 * `{collections}` name what it reads, as `live` takes them.
 *
 * The subscription is keyed by the query's text and parameters, so a query
 * built again on every render -- the natural way to write one -- does not
 * subscribe again unless it asks something else. Rows of the query it asked
 * before are not handed out as this one's: it is `undefined` again until the
 * new one answers. A query that fails throws while rendering, to the nearest
 * error boundary.
 */
export function useLiveQuery(query, opts = {}) {
  const provided = useContext(FenecContext);
  const db = query?.context?.live ? query.context : provided;
  const reads = opts.collections ?? null;
  const key = query
    ? JSON.stringify([typeof query === 'string' ? [query, []] : Array.isArray(query) ? query : query.toFenecQL(), reads])
    : null;
  const [state, setState] = useState({ key: null, rows: undefined, error: null });

  useEffect(() => {
    if (!query) return undefined;
    if (!db?.live) {
      setState({ key, rows: undefined, error: new Error('useLiveQuery: no database for this query (a FenecProvider, or db.from(...))') });
      return undefined;
    }
    // A run already under way when the component goes answers into nothing.
    let on = true;
    const stop = db.live(query, (rows) => on && setState({ key, rows, error: null }), {
      ...(reads && { collections: reads }),
      onError: (error) => on && setState({ key, rows: undefined, error }),
    });
    return () => {
      on = false;
      stop();
    };
    // The query itself is left out on purpose: `key` is what it asks.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [db, key]);

  if (state.key !== key) return undefined;
  if (state.error) throw state.error;
  return state.rows;
}
