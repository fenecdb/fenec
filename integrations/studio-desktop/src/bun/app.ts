// What the app does: open a file (a fenec-server of its own and a window on
// it), open a saved server (a window on its own studio), make a new database,
// keep the list of saved servers. The windows' pages and the menu call these;
// none of it knows about either.
//
// A window per open thing, keyed `file:<real path>` or `remote:<id>`.

import { accessSync, closeSync, constants, existsSync, openSync, realpathSync, statSync, unlinkSync } from "node:fs";
import { basename, join } from "node:path";
import type { BrowserWindow } from "electrobun/main";
import type { ConnectionForm, Notice, Overview, Result } from "../shared/rpc.ts";
import { alreadyHereMessage, diedNotice, heldNotice, internalError, missingFile, notAFile, remoteNotice, startFailedNotice } from "./messages.ts";
import { databaseFileName } from "./names.ts";
import { originOf, probeRemote } from "./remote.ts";
import { secrets } from "./secrets.ts";
import { AlreadyOpen, startFileServer, type FileServer } from "./server.ts";
import { store } from "./store.ts";
import { openStudioWindow, showNotice } from "./windows.ts";

type Open = { key: string; title: string; win: BrowserWindow; server?: FileServer };

/** The open windows. Read by the pages (what is open), the menu and the test hooks. */
export const windows = new Map<string, Open>();
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

/** A message window; a button besides OK calls `onAction` with its id. */
export function notify(n: Notice, onAction?: (id: string) => void) {
	void showNotice(n, onAction);
}

const failed = (message: string): Result => ({ ok: false, message, shown: true });

// ------------------------------------------------------------------ files

/** Opens `raw` in a window of its own, unless it is open already (then that window is shown). */
export function openFile(raw: string): Promise<Result> {
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
		notify(startFailedNotice(name, said), (id) => id === "retry" && void openFile(path));
		return failed(`Could not open ${name}.`);
	}
	try {
		const win = await openStudioWindow({
			key,
			title: `${name} — fenec studio`,
			session: { origin: server.origin, token: server.token, tenant: null },
		});
		track({ key, title: name, win, server }, path);
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

/** Wires a window to what must follow its closing, and the server's dying. */
function track(entry: Open, path: string) {
	windows.set(entry.key, entry);
	entry.win.on("close", () => {
		windows.delete(entry.key);
		// Not awaited by the window, which is gone; awaited by a quit (stopAll).
		if (entry.server) {
			const stopped = entry.server
				.stop()
				.then((code) => fact("server-stopped", { key: entry.key, code }))
				.catch((e) => console.error("could not stop the server", e))
				.finally(() => closing.delete(entry.key));
			closing.set(entry.key, stopped);
		}
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

/** Stops every server and waits for them: a quit does this before the process goes. */
export async function stopAll(): Promise<void> {
	quitting = true;
	await Promise.all([...[...windows.values()].map((w) => w.server?.stop()), ...closing.values()]);
}

export function anyServerRunning(): boolean {
	return [...windows.values()].some((w) => w.server);
}

// ------------------------------------------------------------------ a new database

export type Created = Result & { path?: string };

/**
 * A database that does not exist yet: `name` in `folder`. The name is refused
 * if the folder already holds a file by it, atomically (`wx`), so a file is
 * never made over. The file is begun empty, which fenec-server takes as a new
 * database, and opened as any other.
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
	const result = await openFile(path);
	if (!result.ok) {
		// The server never took it: do not leave an empty file the user did not ask to keep.
		if (statSync(path, { throwIfNoEntry: false })?.size === 0) unlinkSync(path);
		return result;
	}
	return { ...result, path };
}

// ------------------------------------------------------------------ saved servers

export async function openConnection(id: string): Promise<Result> {
	const c = store.connections().find((x) => x.id === id);
	if (!c) {
		notify({ title: "That connection is gone", message: "It was removed." });
		return failed("That connection is gone.");
	}
	const key = `remote:${c.id}`;
	const here = windows.get(key);
	if (here) {
		here.win.activate();
		return { ok: true };
	}
	let origin: string;
	try {
		origin = originOf(c.url);
	} catch (err) {
		notify(internalError(`The address saved for ${c.name} is not valid.`, err), (a) => a === "edit" && openEditor(c.id));
		return failed(`The address saved for ${c.name} is not valid.`);
	}
	let token: string;
	try {
		token = (await secrets.get(`connection:${c.id}`)) ?? "";
	} catch (err) {
		notify(internalError(`The token saved for ${c.name} could not be read from the keychain.`, err));
		return failed("The keychain did not give the token.");
	}
	// Say what is wrong in words before a blank window does.
	const probe = await probeRemote(origin, token, c.tenant);
	if (probe.kind !== "ok") {
		notify(remoteNotice(c.name, origin, probe), (a) => a === "edit" && openEditor(c.id));
		return failed(`Could not open ${c.name}.`);
	}
	try {
		const win = await openStudioWindow({ key, title: `${c.name} — fenec studio`, session: { origin, token, tenant: c.tenant } });
		track({ key, title: c.name, win }, "");
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
	// The token goes to the keychain first and nowhere else on disk; a
	// failure there leaves the list as it was.
	if (form.token) await secrets.set(`connection:${id}`, form.token);
	store.saveConnection({ id, name: form.name.trim() || new URL(origin).host, url: origin, tenant: form.tenant.trim() || null });
	changed();
	return id;
}

export async function removeConnection(id: string) {
	store.removeConnection(id);
	await secrets.delete(`connection:${id}`);
	changed();
}

// ------------------------------------------------------------------ the launcher's view

export async function overview(): Promise<Overview> {
	const connections = await Promise.all(
		store.connections().map(async (c) => ({
			...c,
			hasToken: await secrets
				.get(`connection:${c.id}`)
				.then((t) => (t ?? "") !== "")
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
