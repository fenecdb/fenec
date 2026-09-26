#!/usr/bin/env bash
# The three packages as the registries would take them, installed where a
# user would install them, and used: PyPI's `fenecdb`, npm's `@fenecdb/web`
# and `@fenecdb/react`. Needs `make wasm wasm-lite` first, since the web
# package carries both modules. Run by `make packages`, by CI, and by
# packages.yml before anything is published.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
python="${PYTHON:-python3}"
out="${1:-$root/target/packages}"
rm -rf "$out"
mkdir -p "$out"
out="$(cd "$out" && pwd)"
mkdir -p "$out/dist" "$out/npm"

for f in fenec.wasm fenec-lite.wasm; do
  [ -f "$root/web/$f" ] || { echo "web/$f is missing: make wasm wasm-lite" >&2; exit 1; }
done
[ -d "$root/web/collate" ] || { echo "web/collate is missing: make wasm" >&2; exit 1; }

# Every package says the workspace's version, or a release would publish
# one under another's number.
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
py="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/integrations/python/pyproject.toml" | head -1)"
web="$(node -p "require('$root/web/package.json').version")"
react="$(node -p "require('$root/integrations/react/package.json').version")"
for v in "$py" "$web" "$react"; do
  if [ "$v" != "$version" ]; then
    echo "a package says $v where the workspace says $version (make version V=...)" >&2
    exit 1
  fi
done
if [ -n "${RELEASE_TAG:-}" ] && [ "${RELEASE_TAG#v}" != "$version" ]; then
  echo "the release is $RELEASE_TAG, the packages $version" >&2
  exit 1
fi

# npm: both tarballs, installed into a project of their own, and the web
# client opening the module and its collation data the way its README says.
(cd "$root/web" && npm pack -q --pack-destination "$out/dist" >/dev/null)
(cd "$root/integrations/react" && npm pack -q --pack-destination "$out/dist" >/dev/null)
cd "$out/npm"
npm init -y -q >/dev/null
npm pkg set type=module >/dev/null
npm install -q --no-audit --no-fund \
  "$out/dist/fenecdb-web-$version.tgz" \
  "$out/dist/fenecdb-react-$version.tgz" \
  react@19 >/dev/null
cat > smoke.mjs <<'EOF'
import { readFile } from 'node:fs/promises';
import { Fenec } from '@fenecdb/web';
import { useLiveQuery, FenecProvider } from '@fenecdb/react';

const file = (path) => readFile(new URL(import.meta.resolve(`@fenecdb/web/${path}`)));
for (const module of ['fenec.wasm', 'fenec-lite.wasm']) {
  const db = await Fenec.open(await file(module), {
    collation: (name) => file(`collate/${name}.bin`),
  });
  db.run('create collection people (name text)');
  db.run('put people [{name: "Zeynep"}, {name: "Çağla"}, {name: "Ωμέγα"}, {name: "Жанна"}, {name: "Ömer"}, {name: "Ali"}]');
  // Greek and Cyrillic are not in the module: ordering them fetches their chunks.
  const rows = (await db.query('get people order name collate und')).rows.map((r) => r.name);
  const want = ['Ali', 'Çağla', 'Ömer', 'Zeynep', 'Ωμέγα', 'Жанна'];
  if (JSON.stringify(rows) !== JSON.stringify(want)) throw new Error(`${module}: ${rows}`);
  console.log(`@fenecdb/web ${module}: ${db.version}, ${rows.length} rows in order`);
}
if (typeof useLiveQuery !== 'function' || typeof FenecProvider !== 'function') {
  throw new Error('@fenecdb/react exports are missing');
}
console.log('@fenecdb/react imports');
EOF
node smoke.mjs

# Python: the sdist and the wheel, built and installed in venvs of their
# own -- a system Python is an externally managed one on Debian and
# Homebrew alike, and refuses the build tool.
"$python" -m venv "$out/build"
"$out/build/bin/pip" install -q build
"$out/build/bin/python" -m build -q --outdir "$out/dist" "$root/integrations/python"
"$python" -m venv "$out/py"
"$out/py/bin/pip" install -q "$out/dist/fenecdb-$version-py3-none-any.whl"
"$out/py/bin/python" -c "
import fenecdb
c = fenecdb.Client('http://127.0.0.1:9')
assert c.url == 'http://127.0.0.1:9'
print('fenecdb imports')
"

ls -1 "$out/dist"
