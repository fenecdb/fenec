// A sealed backup of a tenant, taken while it runs: unreadable without the
// key, refused when a byte of it changes, and restored into a database
// whose books balance as the server's do, entry for entry.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { Fenec } from '@fenecdb/web';
import { createRequire } from 'node:module';
import { reconcile } from '../src/reconcile.ts';
import { LocalStore } from '../src/store.ts';
import { freshTenant } from './helpers.ts';
import { mix, prepare } from './workload.ts';

const root = fileURLToPath(new URL('../../..', import.meta.url));
const cli = [process.env.FENEC_CLI, join(root, 'target/release/fenec'), join(root, 'target/debug/fenec')].find((b) => b && existsSync(b));
// The replication token of the node (scripts/db.sh): a backup reads its feed.
const REPLICATION = process.env.FENEC_REPLICATION_TOKEN ?? 'ledger-dev-replication';

test(
  'a sealed backup restores into balanced books, and a changed byte is refused',
  { skip: !cli && !process.env.CI && 'no fenec binary' },
  async () => {
    assert.ok(cli, 'FENEC_CLI, or a fenec build in target/');
    const { operator, ledger } = await freshTenant('backup');
    const l = ledger();
    const world = await prepare(l, operator, { n: 12, frozen: 1 });
    await mix(l, world, { ops: 600, workers: 4, seed: 41 });

    const dir = mkdtempSync(join(tmpdir(), 'ledger-backup-'));
    const key = join(dir, 'key');
    const run = (...args: string[]) => execFileSync(cli!, args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
    run('key', key);
    run('backup', operator.base, join(dir, 'books.sealed'), '--token', REPLICATION, '--key-file', key);

    // Nothing of the books is readable in the file.
    const sealed = readFileSync(join(dir, 'books.sealed'));
    const [anyEntry] = await operator.rows<{ entry: string }>('get journal select entry limit 1');
    assert.equal(sealed.includes(Buffer.from(anyEntry.entry)), false, 'an entry id in the clear');
    assert.equal(sealed.includes(Buffer.from('journal')), false, 'a collection name in the clear');

    // A byte changed, or the wrong key: refused, never read.
    const flipped = Buffer.from(sealed);
    flipped[flipped.length >> 1] ^= 1;
    writeFileSync(join(dir, 'flipped.sealed'), flipped);
    assert.throws(() => run('restore', join(dir, 'flipped.sealed'), join(dir, 'x.fenec'), '--key-file', key));
    run('key', join(dir, 'other'));
    assert.throws(() => run('restore', join(dir, 'books.sealed'), join(dir, 'y.fenec'), '--key-file', join(dir, 'other')));

    // Restored, and opened in this process: the same books, balanced.
    run('restore', join(dir, 'books.sealed'), join(dir, 'books.fenec'), '--key-file', key);
    const wasm = readFileSync(createRequire(import.meta.url).resolve('@fenecdb/web/fenec.wasm'));
    const db = await Fenec.open(wasm);
    db.load(readFileSync(join(dir, 'books.fenec')));
    const restored = new LocalStore(db);
    const report = await reconcile(restored);
    assert.ok(report.ok, JSON.stringify(report).slice(0, 400));
    const live = await reconcile(operator);
    assert.equal(report.entries, live.entries);
    assert.deepEqual(report.currencies, live.currencies);
  },
);
