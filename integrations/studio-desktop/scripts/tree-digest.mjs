// One SHA-256 for a directory: of every file in it (hidden ones too), by path,
// content and whether it is executable. What scripts/electrobun.mjs holds the
// tool's runtime to, since Hutch checks what it fetches only against a lock it
// writes beside the project (.hutch/dependencies.lock, ignored by git, and with
// no hash for its Bun).
//
// A line a file: `<path> NUL <x or -> NUL <sha256>`, sorted by path, joined
// with newlines, hashed. A symlink is its target's text, a path outside the
// tree cannot be read through one. test/tree-digest.test.ts holds it.
import { createHash } from 'node:crypto';
import { lstatSync, readdirSync, readFileSync, readlinkSync } from 'node:fs';
import { join, relative } from 'node:path';

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

/** `{files, tree}` of `root`: how many files, and the digest of them all. */
export function treeDigest(root) {
  const lines = [];
  const walk = (dir) => {
    for (const name of readdirSync(dir).sort()) {
      const full = join(dir, name);
      const rel = relative(root, full);
      const st = lstatSync(full);
      if (st.isDirectory()) walk(full);
      else if (st.isSymbolicLink()) lines.push(`${rel}\0l\0${sha256(readlinkSync(full))}`);
      else if (st.isFile()) lines.push(`${rel}\0${st.mode & 0o111 ? 'x' : '-'}\0${sha256(readFileSync(full))}`);
    }
  };
  walk(root);
  return { files: lines.length, tree: sha256(lines.join('\n')) };
}
