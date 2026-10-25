// What the app does: open a file (a fenec-server of its own and a window on
// it), open a saved server (a window on its own studio), make a new database,
// keep the list of saved servers. The windows' pages and the menu call these;
// none of it knows about either.
//
// A window per open thing, keyed `file:<real path>` or `remote:<id>`.

import { accessSync, closeSync, constants, existsSync, openSync, realpathSync, statSync, unlinkSync } from "node:fs";
import { basename, join } from "node:path";
import { Session, type BrowserWindow } from "electrobun/main";
import type { ConnectionForm, Notice, Overview, Result } from "../shared/rpc.ts";
import { kindOfFile } from "./dbfile.ts";
import {
	alreadyHereMessage,
	diedNotice,
	emptyFile,
	heldNotice,
	internalError,
	missingFile,
	notADatabase,
	notAFile,
	outOfStepNotice,
	remoteNotice,
	startFailedNotice,
} from "./messages.ts";
import { databaseFileName } from "./names.ts";
import { originOf, probeRemote, tokenMayStay } from "./remote.ts";
import { secrets } from "./secrets.ts";
import { AlreadyOpen, StartFailed, startFileServer, type FileServer } from "./server.ts";
import { store, type StoredConnection } from "./store.ts";
import { keychainName, sealToken, tokenFor } from "./token.ts";
import { openStudioWindow, showNotice, type StudioWindow } from "./windows.ts";

type Open = { key: string; title: string; win: BrowserWindow; view: StudioWindow["view"]; partition: string; server?: FileServer; released?: Promise<unknown> };

/** The open windows. Read by the pages (what is open), the menu and the test hooks. */
export const windows = new Map<string, Open>();
/** Opens under way, a file's server being started or a saved server being looked at: a second open of the same thing waits for it. */
const starting = new Map<string, Promise<Result>>();
/** Servers being stopped after their window closed; a quit waits for them, and so does an open of the same file. */
const closing = new Map<string, Promise<unknown>>();
let quitting = false;
const listeners = new Set<() => void>();

const facts = new Set<(what: string, extra: Record<string, unknown>) => void>();
/** What the app did, for whoever listens: the checks (testhooks.ts). Nothing in a shipped app does. */
export const onFact = (f: (what: string, extra: Record<string, unknown>) => void) => void facts.add(f);
const fact = (what: string, extra: Record<string, unknown> = {}) => facts.forEach((f) => f(what, extra));

/** Called whenever a window opens or closes or the recent files change. */
export function onChange(f: () => void) {
	listeners.add(f);
}
const changed = () => listeners.forEach((f) => f());

export const isQuitting = () => quitting;

/** Is anything open, or opening? A file's window exists only once its server is listening. */
export const anythingOpen = () => windows.size > 0 || starting.size > 0;

/**
 * Is there a server a quit must wait for: one running, one being started (it
 * will be running by the time the quit looks), or one whose window closed a
 * moment ago and which is still writing its checkpoint?
 */
export const serversBusy = () => startingFiles().length > 0 || closing.size > 0 || [...windows.values()].some((w) => w.server);

/** The opens under way that start a server (a saved server has none to wait for). */
const startingFiles = () => [...starting].filter(([key]) => key.startsWith("file:")).map(([, run]) => run);

/** A message window; a button besides OK calls `onAction` with its id. */
export function notify(n: Notice, onAction?: (id: string) => void) {
	void showNotice(n, onAction);
}

const failed = (message: string): Result => ({ ok: false, message, shown: true });
/** Nothing new opens once a quit has begun: it would be a server the quit has already counted. */
const goingAway = (): Result => ({ ok: false, message: "The app is quitting.", shown: true });

// ------------------------------------------------------------------ files

/**
 * Opens `raw` in a window of its own, unless it is open already (then that
 * window is shown). `created`: a file this app has just made empty, which is
 * what a database is before its first server; any other empty file is not one.
 */
export function openFile(raw: string, { created = false }: { created?: boolean } = {}): Promise<Result> {
	if (quitting) return Promise.resolve(goingAway());
	let path: string;
	try {
		path = realpathSync(raw);
	} catch {
		notify(missingFile(raw));
		return Promise.resolve(failed(`${raw} does not exist.`));
	}
	if (!statSync(path).isFile()) {
		notify(notAFile(path));
		return Promise.resolve(failed(`${path} is not a file.`));
	}
	const key = `file:${path}`;
	const here = windows.get(key);
	if (here) {
		here.win.activate();
		return Promise.resolve({ ok: true, message: alreadyHereMessage(basename(path)) });
	}
	// A second open of a file this app is still starting a server for must
	// not start a second server: that one would find the first's lock taken
	// and tell the user, wrongly, that another program has the file.
	const pending = starting.get(key);
	if (pending) return pending;
	// Look before a server is started on it: a server makes an empty file a
	// database and leaves `<file>.lock` beside any file it opens, which a file
	// picked by mistake should not get.
	if (!created) {
		try {
			const kind = kindOfFile(path);
			if (kind !== "database") {
				notify(kind === "empty" ? emptyFile(basename(path)) : notADatabase(basename(path)));
				return Promise.resolve(failed(kind === "empty" ? `${basename(path)} is empty.` : `${basename(path)} is not a database.`));
			}
		} catch (err) {
			notify(internalError(`${basename(path)} could not be read.`, err));
			return Promise.resolve(failed(`${basename(path)} could not be read.`));
		}
	}
	// The window of this file was closed a moment ago and its server is still
	// writing its checkpoint, holding the file's lock: a server started now
	// would be refused, and the user told the file is open elsewhere.
	const run = Promise.resolve(closing.get(key))
		.then(() => startFile(path, key))
		.finally(() => starting.delete(key));
	starting.set(key, run);
	return run;
}

async function startFile(path: string, key: string): Promise<Result> {
	const name = basename(path);
	let server: FileServer;
	try {
		server = await startFileServer(path);
	} catch (err) {
		const said = (err as Error).message;
		if (err instanceof AlreadyOpen) {
			notify(heldNotice(name, said), (id) => id === "retry" && void openFile(path));
			return failed(`${name} is open in another program.`);
		}
		// Exit 2 is a flag refused: nothing to retry, the server is not this app's.
		if (err instanceof StartFailed && err.exit === 2) {
			notify(outOfStepNotice(name, said));
			return failed(`${name} was not opened: the server is out of step with the app.`);
		}
		notify(startFailedNotice(name, said), (id) => id === "retry" && void openFile(path));
		return failed(`Could not open ${name}.`);
	}
	try {
		// Not `persist:`: a store that is this window's alone and goes with it.
		const partition = `file-${crypto.randomUUID()}`;
		const studio = await openStudioWindow({
			key,
			title: `${name} — fenec studio`,
			session: { origin: server.origin, token: server.token, tenant: null },
			partition,
		});
		track({ key, title: name, ...studio, partition, server }, path);
	} catch (err) {
		await server.stop();
		notify(internalError(`The window for ${name} could not be opened.`, err));
		return failed(`Could not open ${name}.`);
	}
	fact("server-started", { path, origin: server.origin, pid: server.pid });
	store.touch(path);
	changed();
	return { ok: true };
}

/**
 * Stops an entry's server once, however many times it is asked (its window
 * closing and a quit both do): the promise is the entry's, and in `closing`
 * until it is done.
 */
function release(entry: Open): Promise<unknown> {
	const server = entry.server;
	if (!server) return Promise.resolve();
	if (!entry.released) {
		entry.released = server
			.stop()
			.then((code) => fact("server-stopped", { key: entry.key, code }))
			.catch((e) => console.error("could not stop the server", e))
			.finally(() => closing.delete(entry.key));
		closing.set(entry.key, entry.released);
	}
	return entry.released;
}

/** Wires a window to what must follow its closing, and the server's dying. */
function track(entry: Open, path: string) {
	windows.set(entry.key, entry);
	fact("window-opened", { key: entry.key, partition: entry.partition });
	entry.win.on("close", () => {
		// Only its own entry: a window opened since for the same key is not this one.
		if (windows.get(entry.key) === entry) windows.delete(entry.key);
		// Not awaited by the window, which is gone; awaited by a quit and by an open of the same file.
		void release(entry);
		changed();
	});
	// A server that dies under an open window (killed, crashed, out of disk).
	void entry.server?.exited.then((code) => {
		if (windows.get(entry.key) !== entry || quitting) return;
		const server = entry.server!;
		entry.server = undefined;
		fact("server-died", { key: entry.key, code });
		notify(diedNotice(entry.title, code, server.log()), (id) => id === "retry" && void openFile(path));
		entry.win.close();
	});
}

/**
 * Stops every server and waits for them: a quit does this before the process
 * goes. A server being started is one to stop as well, so the opens under way
 * come first, and then those that are closing.
 */
export async function stopAll(): Promise<void> {
	quitting = true;
	await Promise.allSettled(startingFiles());
	await Promise.all([...[...windows.values()].map(release), ...closing.values()]);
}

// ------------------------------------------------------------------ a new database

export type Created = Result & { path?: string };

/**
 * A database that does not exist yet: `name` in `folder`. The name is refused
 * if the folder already holds a file by it, atomically (`wx`), so a file is
 * never made over. The file is begun empty, which fenec-server takes as a new
 * database, and opened as any other (the one empty file `openFile` takes).
 */
export async function createDatabase(folder: string, typed: string): Promise<Created> {
	const named = databaseFileName(typed);
	if (!named.ok) return { ok: false, message: named.message };
	let dir: string;
	try {
		dir = realpathSync(folder);
		if (!statSync(dir).isDirectory()) throw new Error("not a folder");
		accessSync(dir, constants.W_OK);
	} catch {
		return { ok: false, message: "That folder does not exist or cannot be written to. Choose another." };
	}
	const path = join(dir, named.fileName);
	try {
		closeSync(openSync(path, "wx"));
	} catch (err) {
		if ((err as NodeJS.ErrnoException).code === "EEXIST") {
			return { ok: false, message: `${named.fileName} exists in that folder already. Choose another name, or open it with Open.` };
		}
		return { ok: false, message: `Could not create ${named.fileName}: ${(err as Error).message}` };
	}
	const result = await openFile(path, { created: true });
	if (!result.ok) {
		// The server never took it: do not leave an empty file the user did not ask to keep.
		if (statSync(path, { throwIfNoEntry: false })?.size === 0) unlinkSync(path);
		return result;
	}
	return { ...result, path };
}

// ------------------------------------------------------------------ saved servers

/** Opens a saved server in a window of its own; a second open while the first looks at it waits for it. */
export function openConnection(id: string): Promise<Result> {
	if (quitting) return Promise.resolve(goingAway());
	const c = store.connections().find((x) => x.id === id);
	if (!c) {
		notify({ title: "That connection is gone", message: "It was removed." });
		return Promise.resolve(failed("That connection is gone."));
	}
	const key = `remote:${c.id}`;
	const here = windows.get(key);
	if (here) {
		here.win.activate();
		return Promise.resolve({ ok: true });
	}
	// The look at a server takes up to a few seconds, and gives the user
	// nothing to see: a second click must not open a second window.
	const pending = starting.get(key);
	if (pending) return pending;
	const run = startRemote(c, key).finally(() => starting.delete(key));
	starting.set(key, run);
	return run;
}

async function startRemote(c: StoredConnection, key: string): Promise<Result> {
	let origin: string;
	try {
		origin = originOf(c.url);
	} catch (err) {
		notify(internalError(`The address saved for ${c.name} is not valid.`, err), (a) => a === "edit" && openEditor(c.id));
		return failed(`The address saved for ${c.name} is not valid.`);
	}
	let token: string;
	try {
		// The token for this address: an item saved for another is none (token.ts).
		token = tokenFor(await secrets.get(keychainName(c.id)), origin);
	} catch (err) {
		notify(internalError(`The token saved for ${c.name} could not be read from the keychain.`, err));
		return failed("The keychain did not give the token.");
	}
	// The Keychain took a moment, and the connection may have been edited or
	// removed in it: what was read is for the address that was read it for, and
	// is used only if the connection still names that address.
	const now = store.connections().find((x) => x.id === c.id);
	if (!now || addressOf(now) !== origin) {
		notify({
			title: "That connection changed while it was being opened",
			message: `${c.name} was edited or removed just now, so nothing was sent to the address it had.`,
			hint: "Open it again.",
		});
		return failed(`${c.name} changed while it was being opened.`);
	}
	// Say what is wrong in words before a blank window does.
	const probe = await probeRemote(origin, token, c.tenant);
	if (probe.kind !== "ok") {
		notify(remoteNotice(c.name, origin, probe), (a) => a === "edit" && openEditor(c.id));
		return failed(`Could not open ${c.name}.`);
	}
	try {
		const studio = await openStudioWindow({
			key,
			title: `${c.name} — fenec studio`,
			session: { origin, token, tenant: c.tenant },
			partition: remotePartition(c.id),
		});
		track({ key, title: c.name, ...studio, partition: remotePartition(c.id) }, "");
	} catch (err) {
		notify(internalError(`The window for ${c.name} could not be opened.`, err));
		return failed(`Could not open ${c.name}.`);
	}
	changed();
	return { ok: true };
}

/** Set by the launcher's module: shows the launcher with this connection ready to edit. */
let editor: (id: string) => void = () => {};
export const onEdit = (f: (id: string) => void) => (editor = f);
const openEditor = (id: string) => editor(id);

export async function saveConnection(form: ConnectionForm): Promise<string> {
	const origin = originOf(form.url);
	const id = form.id ?? crypto.randomUUID();
	const previous = store.connections().find((c) => c.id === id);
	if (form.id && !previous) throw new Error("That connection is gone.");
	// A token is for the address it was saved with: pointing the connection
	// somewhere else and leaving the token blank would send the old server's
	// token to the new one, which may be anyone's.
	if (previous && !tokenMayStay(previous.url, origin, form.token)) {
		throw new Error(
			`The address changed from ${previous.url} to ${origin}. The token saved for ${previous.url} is not sent to another address: type the token for ${origin}.`,
		);
	}
	// The token goes to the keychain first and nowhere else on disk; a
	// failure there leaves the list as it was.
	if (form.token) await secrets.set(keychainName(id), sealToken(origin, form.token));
	store.saveConnection({ id, name: form.name.trim() || new URL(origin).host, url: origin, tenant: form.tenant.trim() || null });
	changed();
	return id;
}

/** What a saved server's windows keep (the studio's history, saved queries and theme): a store of its own, kept between runs. */
export const remotePartition = (id: string) => `persist:remote-${id}`;

/**
 * What a store holds that a removal must take: Electrobun 2.0.4's native
 * `sessionClearStorageData` knows these six names and nothing else, and the
 * "all" its TypeScript sends (as `["all"]`) matches none of them, so it does
 * nothing. (The type has no `serviceWorkers` and a `webSQL` the native code
 * does not know, hence the cast.)
 */
const STORE_KINDS = ["cookies", "localStorage", "sessionStorage", "indexedDB", "cache", "serviceWorkers"] as never[];

/** Empties the store a saved server's windows use. */
export function clearStore(id: string) {
	Session.fromPartition(remotePartition(id)).clearStorageData(STORE_KINDS);
}

/** Closes a window and waits until it is closed. */
function closeWindow(entry: Open): Promise<void> {
	return new Promise((resolve) => {
		const gave = setTimeout(resolve, 3000);
		entry.win.on("close", () => {
			clearTimeout(gave);
			resolve();
		});
		entry.win.close();
	});
}

export async function removeConnection(id: string) {
	store.removeConnection(id);
	// A window open on it holds its store, and would write to it again: it goes first.
	const open = windows.get(`remote:${id}`);
	if (open) await closeWindow(open);
	await secrets.delete(keychainName(id));
	// What its windows kept goes with it.
	clearStore(id);
	changed();
}

/** The saved token goes, the connection stays: it is asked for again when it is next needed. */
export async function forgetToken(id: string) {
	await secrets.delete(keychainName(id));
	changed();
}

/** The address a connection names, or "" if what it names is not one. */
function addressOf(c: StoredConnection): string {
	try {
		return originOf(c.url);
	} catch {
		return "";
	}
}

/** The token saved for a connection, for the address it names now; "" if none. */
export async function savedTokenOf(id: string): Promise<{ origin: string; token: string } | null> {
	const c = store.connections().find((x) => x.id === id);
	if (!c) return null;
	const origin = addressOf(c);
	return { origin, token: tokenFor(await secrets.get(keychainName(id)).catch(() => null), origin) };
}

// ------------------------------------------------------------------ the launcher's view

export async function overview(): Promise<Overview> {
	const connections = await Promise.all(
		store.connections().map(async (c) => ({
			...c,
			// A token for the address the connection names now (an item for another is none).
			hasToken: await secrets
				.get(keychainName(c.id))
				.then((item) => tokenFor(item, addressOf(c)) !== "")
				.catch(() => false),
		})),
	);
	return {
		recents: store.recents().map((r) => ({ ...r, name: basename(r.path), exists: existsSync(r.path) })),
		connections,
		open: [...windows.keys()],
		keychain: secrets.kind,
	};
}
