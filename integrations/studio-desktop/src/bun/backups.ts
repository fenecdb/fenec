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
// included -- so it is given a name of the app's own that nothing has, beside
// the new file, and the result is put in place with `link`, which fails if the
// name is taken: no check-then-write window. Pure but for the file system and
// the runner it is handed (Bun's in the app, Node's in test/backups.test.ts,
// which drives the real binaries).

import { copyFileSync, constants, existsSync, linkSync, mkdirSync, readdirSync, rmSync } from "node:fs";
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
	if (existsSync(dest)) return { ok: false, message: `${name} exists in that folder already. Choose another name: a restore never writes over a file.` };
	// A name nothing has, beside the new file: `fenec restore` writes over whatever is at the name it is given.
	const scratch = join(dirname(dest), `.${name}.restoring-${crypto.randomUUID()}`);
	try {
		const to = change === null ? [] : ["--to-change", String(change)];
		const r = await run(["restore", archive.dir, scratch, ...to, ...keyArgs(archive)], childEnvironment(env, {}));
		const m = /change (\d+)/.exec(r.out);
		if (r.code !== 0 || !m) return { ok: false, message: said(r) || `fenec restore ended with ${r.code}` };
		if (kindOfFile(scratch) !== "database") return { ok: false, message: "The restore did not make a database file." };
		try {
			// Fails with EEXIST if the name was taken meanwhile: never over a file.
			linkSync(scratch, dest);
		} catch (e) {
			const code = (e as NodeJS.ErrnoException).code;
			if (code === "EEXIST") return { ok: false, message: `${name} appeared in that folder meanwhile. Choose another name: a restore never writes over a file.` };
			// A disk without hard links (exFAT): a copy that fails as well if the name is taken.
			copyFileSync(scratch, dest, constants.COPYFILE_EXCL);
		}
		return { ok: true, path: dest, change: Number(m[1]) };
	} catch (e) {
		return { ok: false, message: (e as Error).message };
	} finally {
		rmSync(scratch, { force: true });
	}
}

/** `fenec key`: a new key at `path`, which must not exist (the shell refuses to write over a key). */
export async function makeKey(run: Run, path: string, env: Record<string, string | undefined> = process.env): Promise<{ ok: boolean; message: string }> {
	const r = await run(["key", path], childEnvironment(env, {}));
	return { ok: r.code === 0, message: r.code === 0 ? `A new key is in ${path}. Keep it apart from the backups: without it nothing can be restored.` : said(r) };
}
