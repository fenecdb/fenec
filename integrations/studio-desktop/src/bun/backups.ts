// Backups, through the bundled `fenec` shell and nothing of the app's own:
//
//   fenec backup <server> <archive-folder> [--key-file k]   an image of the database, taken while it runs
//   fenec verify <archive-folder> [--key-file k]             what the folder can restore, read as a restore would
//   fenec restore <archive-folder> <new.fenec> [--to-change n] [--key-file k]
//   fenec key <key-file>                                     a new key, readable by its owner alone
//
// A backup speaks the server's replication stream, so it is taken from a
// server: the one serving the file, the one behind its window, or one started
// for the backup alone (app.ts). Its replication token goes in the
// environment, as the server's own did (env.ts). The archive folder is
// `fenec archive`'s layout, so `fenec verify`, `restore` and `prune` read it as
// they read one that command keeps.
//
// **A restore never writes over a file.** `fenec restore` writes its output
// whether or not a file is there -- a database another program serves
// included -- so it writes into a folder of the app's own, made for that one
// restore beside the new file (`mkdtemp`), and the result is put in place with
// `link`, which fails if the name is taken: no check-then-write window. A
// folder and not a name of its own: the shell writes beside its output too
// (`<out>.restoring`, `<out>.compacting`, the lock), and a unique name it
// replaced the end of was shared by two restores to one name, which broke each
// other. Removing the folder takes all of it. Pure but for the file system and
// the runner it is handed (Bun's in the app, Node's in test/backups.test.ts,
// which drives the real binaries).

import { closeSync, constants, copyFileSync, existsSync, fsyncSync, linkSync, mkdirSync, mkdtempSync, openSync, readdirSync, realpathSync, rmSync, statSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { childEnvironment } from "./env.ts";
import { kindOfFile } from "./dbfile.ts";

/** What a run of `fenec` said. `code` null: killed. */
export type Ran = { code: number | null; out: string; err: string };
/** Runs the bundled `fenec` with `args` in environment `env`. */
export type Run = (args: string[], env: Record<string, string>) => Promise<Ran>;

export type Archive = { dir: string; keyFile: string | null };

/** An image a backup put in an archive folder: `image-<change, 20 digits>-<ms>.fenec`. */
export type Image = { change: number; at: number; file: string };

const IMAGE = /^image-(\d{20})-(\d+)\.fenec$/;

/** The images among a folder's names, newest first. */
export function imagesIn(names: string[]): Image[] {
	return names
		.map((n) => IMAGE.exec(n))
		.filter((m): m is RegExpExecArray => m !== null)
		.map((m) => ({ change: Number(m[1]), at: Number(m[2]), file: m[0] }))
		.sort((a, b) => b.at - a.at || b.change - a.change);
}

/** The images in an archive folder, newest first; none if it is not there. */
export function listImages(dir: string): Image[] {
	try {
		return imagesIn(readdirSync(dir));
	} catch {
		return [];
	}
}

const keyArgs = (a: Archive) => (a.keyFile ? ["--key-file", a.keyFile] : []);
/** What `fenec` said last, for a message: its error lines, without the command's name before them. */
export const said = (r: Ran) => (r.err.trim() || r.out.trim()).split("\n").slice(-4).join("\n").replace(/^fenec \w+: /gm, "");

export type Taken = { ok: true; change: number } | { ok: false; message: string };

/**
 * An image of the database `server` serves, into `archive`. The folder is made
 * if it is not there: given a name that is not a folder, `fenec backup` writes a
 * backup *file* by that name instead.
 */
export async function takeBackup(run: Run, server: { local: string; replicationToken: string }, archive: Archive, env: Record<string, string | undefined> = process.env): Promise<Taken> {
	try {
		mkdirSync(archive.dir, { recursive: true });
	} catch (e) {
		return { ok: false, message: `The folder ${archive.dir} could not be made: ${(e as Error).message}` };
	}
	const r = await run(["backup", server.local, archive.dir, ...keyArgs(archive)], childEnvironment(env, { FENEC_REPLICATION_TOKEN: server.replicationToken }));
	const m = /the database at change (\d+)/.exec(r.out);
	if (r.code !== 0 || !m) return { ok: false, message: said(r) || `fenec backup ended with ${r.code}` };
	return { ok: true, change: Number(m[1]) };
}

export type Verified = { ok: boolean; text: string };

/** `fenec verify`: what the folder can restore, or why it cannot. */
export async function verifyArchive(run: Run, archive: Archive, env: Record<string, string | undefined> = process.env): Promise<Verified> {
	const r = await run(["verify", archive.dir, ...keyArgs(archive)], childEnvironment(env, {}));
	return { ok: r.code === 0, text: r.code === 0 ? r.out.trim() : said(r) };
}

export type Restored = { ok: true; path: string; change: number } | { ok: false; message: string };

/**
 * The archive as it stood at image `change` (the newest when null), as a new
 * file at `dest`, which must not exist: if it does, nothing is written there.
 */
export async function restoreArchive(run: Run, archive: Archive, change: number | null, dest: string, env: Record<string, string | undefined> = process.env): Promise<Restored> {
	const name = basename(dest);
	const taken = (meanwhile: boolean): Restored => ({
		ok: false,
		message: `${name} ${meanwhile ? "appeared in that folder meanwhile" : "exists in that folder already"}. Choose another name: a restore never writes over a file.`,
	});
	if (existsSync(dest)) return taken(false);
	// A folder of this restore's own, beside the new file (so a link reaches it):
	// whatever the shell writes, at its output and beside it, is in there.
	let scratch: string;
	try {
		scratch = mkdtempSync(join(dirname(dest), `.${name}.restoring-`));
	} catch (e) {
		return { ok: false, message: (e as Error).message };
	}
	const out = join(scratch, "db.fenec");
	try {
		const to = change === null ? [] : ["--to-change", String(change)];
		const r = await run(["restore", archive.dir, out, ...to, ...keyArgs(archive)], childEnvironment(env, {}));
		const m = /change (\d+)/.exec(r.out);
		if (r.code !== 0 || !m) return { ok: false, message: said(r) || `fenec restore ended with ${r.code}` };
		if (kindOfFile(out) !== "database") return { ok: false, message: "The restore did not make a database file." };
		try {
			// Fails with EEXIST if the name was taken meanwhile: never over a file.
			linkSync(out, dest);
		} catch (e) {
			if ((e as NodeJS.ErrnoException).code === "EEXIST") return taken(true);
			// A disk without hard links (exFAT, FAT32): a copy, which fails as well
			// if the name is taken, and is on the disk before it is said to be done.
			try {
				copyFileSync(out, dest, constants.COPYFILE_EXCL);
			} catch (c) {
				if ((c as NodeJS.ErrnoException).code === "EEXIST") return taken(true);
				throw c;
			}
			const fd = openSync(dest, "r");
			try {
				fsyncSync(fd);
			} finally {
				closeSync(fd);
			}
		}
		return { ok: true, path: dest, change: Number(m[1]) };
	} catch (e) {
		return { ok: false, message: (e as Error).message };
	} finally {
		rmSync(scratch, { recursive: true, force: true });
	}
}

/** `path` resolved through the links of its longest part that exists: a folder not made yet is where it will be. */
function resolved(path: string): string {
	const rest: string[] = [];
	let at = path;
	for (;;) {
		try {
			return join(realpathSync(at), ...rest);
		} catch {
			const up = dirname(at);
			if (up === at) return path;
			rest.unshift(basename(at));
			at = up;
		}
	}
}

/** The device of `path`, or of the longest part of it that exists. */
function deviceOf(path: string): number {
	for (let at = path; ; at = dirname(at)) {
		const s = statSync(at, { throwIfNoEntry: false });
		if (s) return s.dev;
		if (dirname(at) === at) return -1;
	}
}

const within = (path: string, folder: string) => path === folder || path.startsWith(folder.endsWith("/") ? folder : `${folder}/`);

/**
 * Whether `keyFile` is kept apart from the backups in `dir` of the database at
 * `database`: `refuse` says why it is not, `warn` what to look at.
 *
 * Refused **anywhere under the folder that was picked**: the backups' folder,
 * or, where it is named after the database as the app names it (`<picked>/<stem>`),
 * the folder above it, which is the one the user chose and may copy or sync
 * whole. Compared as real paths, through links. Refused as well **on the disk
 * the backups are on when that is not the database's**: backups on a drive of
 * their own are there to be carried off, and the key must not go with them;
 * beside the live database, a key on the same disk shows nothing the database
 * itself does not. A key others may read is a warning, not a refusal: the
 * shell's `fenec key` writes 0600, and one copied in may have lost that.
 */
export function keyPlacement(dir: string, keyFile: string, database: string, dev: (path: string) => number = deviceOf): { refuse?: string; warn?: string } {
	const key = resolved(keyFile);
	const folder = resolved(dir);
	const stem = basename(database).replace(/\.fenec$/i, "");
	const picked = basename(folder) === stem ? dirname(folder) : folder;
	if (within(key, picked)) return { refuse: `The key is inside ${picked}, with the backups: anyone who has that folder could read them. Keep it in another folder.` };
	const backupsDisk = dev(folder);
	if (backupsDisk !== dev(database) && dev(key) === backupsDisk) {
		return { refuse: "The key is on the disk the backups go to, which is not the database's: whoever takes that disk has both. Keep the key on another disk." };
	}
	const mode = statSync(key, { throwIfNoEntry: false })?.mode ?? 0;
	if (mode & 0o077) return { warn: `Others on this Mac may read the key (its permissions are ${(mode & 0o777).toString(8)}): chmod 600 it, as fenec key makes one.` };
	return {};
}

/** `fenec key`: a new key at `path`, which must not exist (the shell refuses to write over a key). */
export async function makeKey(run: Run, path: string, env: Record<string, string | undefined> = process.env): Promise<{ ok: boolean; message: string }> {
	const r = await run(["key", path], childEnvironment(env, {}));
	return { ok: r.code === 0, message: r.code === 0 ? `A new key is in ${path}. Keep it apart from the backups: without it nothing can be restored.` : said(r) };
}
