// The playground: fenec studio -- the same files fenec-server serves with
// `--studio` -- over a database in this tab instead of a server's. The
// browser module runs in a worker (studio/worker.js) and answers the studio
// as a server would; this module seeds it and lists the queries to try.
//
// build.py serves it beside the studio's modules (dist/studio/), every name
// rewritten to its hashed copy: `./app.js` and `./local.js` are the
// studio's, `../fenec.wasm` and `../collate/` the site's own module, and
// `./playground-data.js` the seed and the examples.

import { Local } from './local.js';
import { local } from './app.js';
import { seed, EXAMPLES } from './playground-data.js';

// ------------------------------------------------------------------ start

const app = document.getElementById('app');
app.textContent = 'Opening a database in this tab…';
try {
  const db = await Local.open({
    wasm: new URL('../fenec.wasm', import.meta.url).href,
    collation: new URL('../collate/', import.meta.url).href,
    seed,
    docs: new URL('../docs/studio', import.meta.url).href,
  });
  app.classList.remove('booting');
  await local(db, { examples: EXAMPLES, first: { view: 'query', collection: 'products', example: 0 } });
} catch (e) {
  app.classList.remove('booting');
  app.classList.add('failed');
  app.textContent = `This browser could not open the database: ${e?.message ?? e}. It needs WebAssembly with SIMD and module workers: Chrome 91, Firefox 114, Safari 16.4 or later.`;
}
