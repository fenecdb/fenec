// Builds what the browser loads: the dashboard's script (client/app.ts,
// with the charts it shares with the server) and the tracker (client/
// tracker.ts), minified, and the font copied out of its package. Prints
// each one's size, compressed as a server would send it.
import { build } from 'esbuild';
import { copyFileSync, mkdirSync, readFileSync, statSync } from 'node:fs';
import { brotliCompressSync, gzipSync } from 'node:zlib';

const out = (f: string) => new URL(`../public/${f}`, import.meta.url).pathname;
mkdirSync(out('fonts'), { recursive: true });
for (const [entry, file] of [
  ['client/app.ts', 'app.js'],
  ['client/tracker.ts', 'k.js'],
] as const) {
  await build({ entryPoints: [entry], outfile: out(file), bundle: true, minify: true, format: 'iife', target: 'es2020', legalComments: 'none' });
}
copyFileSync(new URL('../node_modules/@fontsource-variable/archivo/files/archivo-latin-wdth-normal.woff2', import.meta.url), out('fonts/archivo.woff2'));
for (const f of ['app.js', 'k.js', 'fonts/archivo.woff2']) {
  const b = readFileSync(out(f));
  console.log(`${f.padEnd(20)} ${String(statSync(out(f)).size).padStart(6)} B, ${gzipSync(b, { level: 9 }).length} B gzip, ${brotliCompressSync(b).length} B brotli`);
}
