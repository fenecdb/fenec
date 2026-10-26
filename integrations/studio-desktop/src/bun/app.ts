// What the app does: open a file (a fenec-server of its own and a window on
// it), serve a file to the apps on this Mac or the network, open a saved
// server (a window on its own studio), make a new database, take and restore
// backups, keep the lists. The windows' pages and the menu call these; none of
// it knows about either.
//
// **One server a file, never two.** A file has at most one server of this
// app's at a time: its window's, its served one, or one started for a backup
// or a compaction alone; whatever starts or stops one does it in the file's
// lane (`inLane`), one after another, and a window on a served database is a
// window on that server, not a second one. A server of another program's is
// found by `fenec-server` itself, which takes `<file>.lock` and refuses to
// open a file whose lock is taken: the app says so and starts nothing.
//
// A window per open thing, keyed `file:<real path>` or `remote:<id>`.

import { accessSync, closeSync, constants, existsSync, openSync, realpathSync, statSync, unlinkSync } from "node:fs";
import { createServer } from "node:net";
import { hostname, networkInterfaces } from "node:os";
import { basename, dirname, isAbsolute, join } from "node:path";
import { Session, Utils, type BrowserWindow } from "electrobun/main";
import type { Check, ConnectionForm, DatabaseRow, Figures, NewDbForm, Notice, Overview, Result, ServedRow } from "../shared/rpc.ts";
import { listImages, makeKey as makeKeyFile, restoreArchive, takeBackup, verifyArchive, type Archive } from "./backups.ts";
import { kindOfFile } from "./dbfile.ts";
import {
	SYNC_OFF_BACKUP,
	alreadyHereMessage,
	diedNotice,
	emptyFile,
	heldNotice,
	internalError,
	missingFile,
	notADatabase,
	notAFile,
	outOfStepNotice,
	portTakenNotice,
	remoteNotice,
	servedDiedNotice,
	startFailedNotice,
} from "./messages.ts";
import { between, readMetrics, type Sample } from "./metrics.ts";
import { portAnswers } from "./process.ts";
import { collectionStatement, databaseFileName } from "./names.ts";
import { originOf, probeRemote } from "./remote.ts";
import { secrets } from "./secrets.ts";
import { AlreadyOpen, PortTaken, StartFailed, randomToken, runCli, serverBinary, startFileServer, type FileServer } from "./server.ts";
import { commandShown, factsOf, hostOf, servePlan, snippets, taskPlan, windowPlan, type ServeSettings, type Sync } from "./serving.ts";
import { store, type StoredConnection, type StoredDatabase } from "./store.ts";
import { keychainName, sealToken, serveItemName, servedToken, tokenFor } from "./token.ts";
import { openStudioWindow, showNotice, type StudioWindow } from "./windows.ts";

type Open = {
	key: string;
	title: string;
	win: BrowserWindow;
	view: StudioWindow["view"];
	partition: string;
	/** The window's own server, stopped with it. */
	server?: FileServer;
	/** The served database's server it is a window on, which goes on when it closes. */
	on?: Served;
	released?: Promise<unknown>;
};

/** A database the app serves: its server, and what it was started with. */
type Served = { path: string; name: string; server: FileServer; settings: ServeSettings; sync: Sync; state: "running" | "stopping" };

/** The open windows. Read by the pages (what is open), the menu and the test hooks. */
export const windows = new Map<string, Open>();
/** Opens under way, a file's server being started or a saved server being looked at: a second open of the same thing waits for it. */
const starting = new Map<string, Promise<Result>>();
/** Window servers being stopped after their window closed; a quit waits for them, and so does anything that starts one on the file. */
const closing = new Map<string, Promise<unknown>>();
/** The served databases, by real path. */
export const served = new Map<string, Served>();
/** Served databases whose server is being started. */
const serveStarting = new Set<string>();
/** Servers started for a backup or a compaction alone, by real path. */
const tasks = new Map<string, FileServer>();
/** Each file's lane: what starts or stops a server on it runs here, one after another. */
const lanes = new Map<string, Promise<unknown>>();
/** The last metrics read from each server, for its rates. */
const samples = new WeakMap<FileServer, Sample>();
/** The last "Check now" of each saved connection, this run. */
const checks = new Map<string, Check>();
let quitting = false;
const listeners = new Set<() => void>();

const facts = new Set<(what: string, extra: Record<string, unknown>) => void>();
/** What the app did, for whoever listens: the checks (testhooks.ts). Nothing in a shipped app does. */
export const onFact = (f: (what: string, extra: Record<string, unknown>) => void) => void facts.add(f);
const fact = (what: string, extra: Record<string, unknown> = {}) => facts.forEach((f) => f(what, extra));

/** Called whenever a window opens or closes, a server starts or stops, or the lists change. */
export function onChange(f: () => void) {
	listeners.add(f);
}
const changed = () => listeners.forEach((f) => f());

export const isQuitting = () => quitting;

/** Is anything open, or opening? A file's window exists only once its server is listening. */
export const anythingOpen = () => windows.size > 0 || starting.size > 0;

/**
 * Is there a server a quit must wait for: one running (a window's, a served
 * database's, one taking a backup), one being started, or one whose window
 * closed a moment ago and which is still writing its checkpoint?
 */
export const serversBusy = () =>
	startingFiles().length > 0 || closing.size > 0 || served.size > 0 || tasks.size > 0 || lanes.size > 0 || [...windows.values()].some((w) => w.server);

/** The opens under way that start a server (a saved server has none to wait for). */
const startingFiles = () => [...starting].filter(([key]) => key.startsWith("file:")).map(([, run]) => run);

/** Runs `f` after everything already in `path`'s lane, and before anything put there after it. */
function inLane<T>(path: string, f: () => Promise<T>): Promise<T> {
	const run = (lanes.get(path) ?? Promise.resolve()).then(f, f);
	const tail = run.then(
		() => {},
		() => {},
	);
	lanes.set(path, tail);
	void tail.then(() => lanes.get(path) === tail && lanes.delete(path));
	return run;
}

/** A message window; a button besides OK calls `onAction` with its id. */
export function notify(n: Notice, onAction?: (id: string) => void) {
	void showNotice(n, onAction);
}

const failed = (message: string): Result => ({ ok: false, message, shown: true });
/** Nothing new opens once a quit has begun: it would be a server the quit has already counted. */
const goingAway = (): Result => ({ ok: false, message: "The app is quitting.", shown: true });

/** `raw` as the real path of a database file, or the reason it is not one, said in a notice. */
function databaseAt(raw: string, empty = false): { path: string } | Result {
	let path: string;
	try {
		path = realpathSync(raw);
	} catch {
		notify(missingFile(raw));
		return failed(`${raw} does not exist.`);
	}
	if (!statSync(path).isFile()) {
		notify(notAFile(path));
		return failed(`${path} is not a file.`);
	}
	// Look before a server is started on it: a server makes an empty file a
	// database and leaves `<file>.lock` beside any file it opens, which a file
	// picked by mistake should not get. The one empty file a server is started
	// on is one New database has just made.
	try {
		const kind = kindOfFile(path);
		if (kind === "database" || (empty && kind === "empty")) return { path };
		notify(kind === "empty" ? emptyFile(basename(path)) : notADatabase(basename(path)));
		return failed(kind === "empty" ? `${basename(path)} is empty.` : `${basename(path)} is not a database.`);
	} catch (err) {
		notify(internalError(`${basename(path)} could not be read.`, err));
		return failed(`${basename(path)} could not be read.`);
	}
}

/** What a server that could not start is told as, with what to do next. */
function startNotice(name: string, err: unknown, again: () => void, settings?: () => void): Result {
	const said = (err as Error).message;
	if (err instanceof AlreadyOpen) {
		notify(heldNotice(name, said), (id) => id === "retry" && again());
		return failed(`${name} is open in another program.`);
	}
	if (err instanceof PortTaken) {
		notify(portTakenNotice(name, err.port, said), (id) => id === "settings" && settings?.());
		return failed(`Port ${err.port} is taken.`);
	}
	// Exit 2 is a flag refused: nothing to retry, the server is not this app's.
	if (err instanceof StartFailed && err.exit === 2) {
		notify(outOfStepNotice(name, said));
		return failed(`${name} was not opened: the server is out of step with the app.`);
	}
	notify(startFailedNotice(name, said), (id) => id === "retry" && again());
	return failed(`Could not open ${name}.`);
}

// ------------------------------------------------------------------ files

/** Opens `raw` in a window of its own, unless it is open already (then that window is shown). */
export function openFile(raw: string): Promise<Result> {
	if (quitting) return Promise.resolve(goingAway());
	const at = databaseAt(raw);
	if (!("path" in at)) return Promise.resolve(at);
	const { path } = at;
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
		.then(() =>
			inLane(path, async () => {
				if (windows.get(key)) return { ok: true };
				const s = served.get(path);
				return s?.state === "running" ? openOnServed(s) : startFile(path, key);
			}),
		)
		.finally(() => starting.delete(key));
	starting.set(key, run);
	return run;
}

async function startFile(path: string, key: string): Promise<Result> {
	const name = basename(path);
	const d = store.updateDatabase(path);
	let server: FileServer;
	try {
		// A token for this start alone, in the server's environment and the window's preload.
		server = await startFileServer(windowPlan(path, d.sync), randomToken());
	} catch (err) {
		return startNotice(name, err, () => void openFile(path));
	}
	try {
		// Not `persist:`: a store that is this window's alone and goes with it.
		const partition = `file-${crypto.randomUUID()}`;
		const studio = await openStudioWindow({
			key,
			title: `${name} — fenec studio`,
			session: { origin: server.local, token: server.token, tenant: null },
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
	void soonFigures(server, path);
	changed();
	return { ok: true };
}

/** A window on the server that serves `s`, signed in with its token: no second server. */
async function openOnServed(s: Served): Promise<Result> {
	const key = `file:${s.path}`;
	try {
		const partition = `file-${crypto.randomUUID()}`;
		const studio = await openStudioWindow({
			key,
			title: `${s.name} — fenec studio`,
			session: { origin: s.server.local, token: s.server.token, tenant: null },
			partition,
		});
		track({ key, title: s.name, ...studio, partition, on: s }, s.path);
	} catch (err) {
		notify(internalError(`The window for ${s.name} could not be opened.`, err));
		return failed(`Could not open ${s.name}.`);
	}
	fact("window-on-served", { path: s.path, origin: s.server.local });
	store.touch(s.path);
	changed();
	return { ok: true };
}

/**
 * Stops an entry's server once, however many times it is asked (its window
 * closing and a quit both do): the promise is the entry's, and in `closing`
 * until it is done. A window on a served database stops nothing.
 */
function release(entry: Open): Promise<unknown> {
	const server = entry.server;
	if (!server) return Promise.resolve();
	if (!entry.released) {
		entry.released = server
			.stop()
			.then((code) => fact("server-stopped", { key: entry.key, code }))
			.catch((e) => console.error("could not stop the server", e))
			.finally(() => {
				closing.delete(entry.key);
				changed();
			});
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
	// A served database's server dying is `watchServed`'s to tell.
	void entry.server?.exited.then((code) => {
		if (windows.get(entry.key) !== entry || quitting) return;
		const server = entry.server!;
		entry.server = undefined;
		fact("server-died", { key: entry.key, code });
		notify(diedNotice(entry.title, code, server.log()), (id) => id === "retry" && void openFile(path));
		entry.win.close();
	});
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

/**
 * Stops every server and waits for them: a quit does this before the process
 * goes. What is under way in a lane (a server being started) finishes first,
 * so it is one to stop as well; then the windows', the served ones, those
 * taking a backup, and those that are closing.
 */
export async function stopAll(): Promise<void> {
	quitting = true;
	await Promise.allSettled([...startingFiles(), ...lanes.values()]);
	await Promise.all([
		...[...windows.values()].map(release),
		...[...served.values()].map((s) => stopServing(s, "the app quit")),
		...[...tasks.values()].map((t) => t.stop()),
		...closing.values(),
	]);
}

// ------------------------------------------------------------------ serving

/** The served database's figures for the Servers list, what its window and its apps reach. */
function servedRow(d: StoredDatabase): ServedRow | null {
	if (!d.serve) return null;
	const s = served.get(d.path);
	const settings = s?.settings ?? d.serve;
	const sync = s?.sync ?? d.sync;
	const plan = servePlan(d.path, sync, settings);
	const running = s?.state === "running";
	const url = running ? s.server.local : null;
	return {
		state: s ? s.state : serveStarting.has(d.path) ? "starting" : "stopped",
		settings,
		url,
		networkUrls: running && settings.reach === "lan" ? networkUrls(s.server.port) : [],
		metricsUrl: running && s.server.metrics ? `${s.server.metrics}/_metrics` : null,
		startedAt: s ? s.server.startedAt : null,
		lastRan: d.lastRan ?? null,
		facts: factsOf(settings, sync),
		command: commandShown(plan, Utils.paths.home),
		snippets: url ? snippets(url, d.seen?.first || "orders") : null,
	};
}

/** Where another machine reaches a server on the network: this Mac's name, and its addresses. */
function networkUrls(port: number): string[] {
	const name = hostname();
	const urls = [`http://${name.endsWith(".local") ? name : `${name}.local`}:${port}`];
	for (const list of Object.values(networkInterfaces())) {
		for (const a of list ?? []) if (a.family === "IPv4" && !a.internal) urls.push(`http://${a.address}:${port}`);
	}
	return urls;
}

/**
 * Serves `raw` with `settings`, its token the one the Keychain keeps for it
 * (made and kept first if there is none). A window of the file's own server
 * is closed first, its server stopped, and opened again on the served one: one
 * server a file. `reopen`: open a window on it once it runs, as one was before.
 */
export function serveFile(raw: string, settings: ServeSettings, sync: Sync, { reopen = false }: { reopen?: boolean } = {}): Promise<Result> {
	if (quitting) return Promise.resolve(goingAway());
	const at = databaseAt(raw);
	if (!("path" in at)) return Promise.resolve(at);
	const { path } = at;
	const name = basename(path);
	const key = `file:${path}`;
	return inLane(path, async (): Promise<Result> => {
		if (quitting) return goingAway();
		if (served.has(path)) return { ok: false, message: `${name} is served already: stop it, or change its settings.` };
		let again = reopen;
		const here = windows.get(key);
		if (here?.server) {
			again = true;
			await closeWindow(here);
		}
		await closing.get(key);
		const d = store.updateDatabase(path, (x) => {
			x.sync = sync;
			x.serve = settings;
		});
		let token: string;
		try {
			token = await servedToken(secrets, d.id, path, randomToken);
		} catch (err) {
			notify(internalError(`The token for ${name} could not be kept in the keychain, so it was not served.`, err));
			return failed("The keychain did not take the token.");
		}
		let server: FileServer;
		serveStarting.add(path);
		changed();
		try {
			server = await startFileServer(servePlan(path, sync, settings), token);
		} catch (err) {
			serveStarting.delete(path);
			store.updateDatabase(path, (x) => (x.lastRan = { at: Date.now(), port: settings.port, why: "could not start" }));
			changed();
			return startNotice(
				name,
				err,
				() => void serveFile(path, settings, sync, { reopen: again }),
				() => askToServe(path),
			);
		}
		serveStarting.delete(path);
		const s: Served = { path, name, server, settings, sync, state: "running" };
		served.set(path, s);
		watchServed(s);
		fact("served", { path, origin: server.origin, local: server.local, port: server.port, pid: server.pid, metrics: server.metrics });
		store.touch(path);
		void soonFigures(server, path);
		changed();
		// Not awaited: the open goes in the lane after this.
		if (again) void openFile(path);
		return { ok: true, message: `${name} is served at ${server.local}.` };
	});
}

/** A served database's server ending without being asked to: said, and its windows closed. */
function watchServed(s: Served) {
	void s.server.exited.then((code) => {
		if (served.get(s.path) !== s || s.state === "stopping" || quitting) return;
		served.delete(s.path);
		store.updateDatabase(s.path, (d) => (d.lastRan = { at: Date.now(), port: s.server.port, why: `exited (${code === null ? "killed" : `code ${code}`})` }));
		fact("served-died", { path: s.path, code });
		for (const w of [...windows.values()]) if (w.on === s) w.win.close();
		notify(servedDiedNotice(s.name, code, s.server.log()), (id) => id === "start" && void serveFile(s.path, s.settings, s.sync));
		changed();
	});
}

/** Stops a served database's server, the one shutdown: SIGTERM, then SIGKILL only 30 s later. Its windows close first. */
async function stopServing(s: Served, why: string): Promise<number | null> {
	s.state = "stopping";
	changed();
	for (const w of [...windows.values()]) if (w.on === s) await closeWindow(w);
	const code = await s.server.stop();
	if (served.get(s.path) === s) served.delete(s.path);
	store.updateDatabase(s.path, (d) => (d.lastRan = { at: Date.now(), port: s.server.port, why }));
	fact("served-stopped", { path: s.path, code, why });
	changed();
	return code;
}

export function stopServed(path: string): Promise<Result> {
	return inLane(path, async () => {
		const s = served.get(path);
		if (!s) return { ok: true };
		const code = await stopServing(s, "stopped");
		return { ok: true, message: `${s.name} stopped${code === 0 ? "" : ` (exit ${code})`}.` };
	});
}

/** Stops it and starts it again: with new settings, or the ones it ran with. Its window comes back if it had one. */
export async function restartServed(path: string, settings?: ServeSettings, sync?: Sync): Promise<Result> {
	const s = served.get(path);
	const d = store.database(path);
	const use = settings ?? s?.settings ?? d?.serve;
	if (!use) return { ok: false, message: "It has never been served." };
	const reopen = [...windows.values()].some((w) => w.on && w.on.path === path);
	await stopServed(path);
	return serveFile(path, use, sync ?? s?.sync ?? d?.sync ?? "250", { reopen });
}

/** A stopped server out of the Servers list: its settings and its token go. */
export async function removeServed(path: string): Promise<void> {
	if (served.has(path)) return;
	const d = store.database(path);
	if (!d) return;
	store.updateDatabase(path, (x) => {
		delete x.serve;
		delete x.lastRan;
	});
	await secrets.delete(serveItemName(d.id));
	changed();
}

/** A served database's token, for Show and Copy: the kept one, or one made and kept now. */
export async function serveTokenOf(path: string): Promise<string> {
	const d = store.updateDatabase(path);
	return servedToken(secrets, d.id, path, randomToken);
}

/** A new token for a served database, kept now and used from its next start. */
export async function newServeToken(path: string): Promise<Result> {
	const d = store.updateDatabase(path);
	await servedToken(secrets, d.id, path, randomToken, true);
	fact("token-renewed", { path });
	return { ok: true, message: served.has(path) ? "A new token is kept. The running server has the old one until it starts again: restart it, then give your apps the new one." : "A new token is kept for the next start." };
}

/**
 * Is `port` free on the host `reach` names? Nothing may answer on it at
 * 127.0.0.1, whatever a bind says (process.ts), and then a bind there, let go
 * at once, says what the server's own would.
 */
export async function portFree(port: number, reach: "mac" | "lan", path: string): Promise<{ free: boolean; text: string }> {
	const s = served.get(path);
	if (s && s.server.port === port) return { free: true, text: "This server's own port." };
	if (await portAnswers(port)) return { free: false, text: `Taken: another program answers on ${port}.` };
	return new Promise((resolve) => {
		const probe = createServer();
		probe.once("error", (e: NodeJS.ErrnoException) =>
			resolve({ free: false, text: e.code === "EADDRINUSE" ? `Taken: another program listens on ${port}.` : `Cannot be used: ${e.message}.` }),
		);
		probe.listen({ port, host: hostOf(reach), exclusive: true }, () => probe.close(() => resolve({ free: true, text: "Free now." })));
	});
}

/** The output a served database's server wrote, its last lines. */
export function serverLog(path: string): string[] {
	return served.get(path)?.server.lines() ?? [];
}

/** Has the main window open the serve sheet on a database (set by pages.ts). */
let serveSheet: (path: string) => void = () => {};
export const onServeSheet = (f: (path: string) => void) => (serveSheet = f);
const askToServe = (path: string) => serveSheet(path);

// ------------------------------------------------------------------ a server for a moment

/**
 * Runs `f` with a server on the file: the one serving it, the one behind its
 * window, or, if nothing runs on it, one started for `f` alone (no studio, this
 * Mac, any port) and stopped once `f` is done. `empty`: the file is one New
 * database has just made, which the server makes a database.
 */
async function withServer<T extends Result>(raw: string, f: (server: FileServer) => Promise<T>, { empty = false } = {}): Promise<T | Result> {
	if (quitting) return goingAway();
	const at = databaseAt(raw, empty);
	if (!("path" in at)) return at;
	const { path } = at;
	const running = () => {
		const s = served.get(path);
		if (s?.state === "running") return s.server;
		return windows.get(`file:${path}`)?.server;
	};
	const now = running();
	if (now) return f(now);
	return inLane(path, async () => {
		// The lane may have waited behind a start: look again.
		const since = running();
		if (since) return f(since);
		await closing.get(`file:${path}`);
		let server: FileServer;
		try {
			server = await startFileServer(taskPlan(path), randomToken());
		} catch (err) {
			if (err instanceof AlreadyOpen) return { ok: false, message: `${basename(path)} is open in another program: close it there first.` };
			return { ok: false, message: `A server could not be started on ${basename(path)}: ${(err as Error).message.split("\n").slice(-2).join(" ")}` };
		}
		tasks.set(path, server);
		fact("task-server-started", { path, pid: server.pid });
		changed();
		try {
			return await f(server);
		} finally {
			await figures(server, path).catch(() => null);
			const code = await server.stop();
			tasks.delete(path);
			fact("task-server-stopped", { path, code });
			changed();
		}
	});
}

/** A statement run on `server` with its token, as any client runs one. */
async function query(server: FileServer, statement: string): Promise<{ ok: boolean; text: string }> {
	try {
		const res = await fetch(`${server.local}/query`, {
			method: "POST",
			headers: { authorization: `Bearer ${server.token}`, "content-type": "application/json" },
			body: JSON.stringify({ query: statement }),
			redirect: "manual",
			signal: AbortSignal.timeout(600_000),
		});
		const body = (await res.json().catch(() => ({}))) as { error?: string; message?: string };
		return res.ok ? { ok: true, text: body.message ?? "" } : { ok: false, text: body.error ?? `HTTP ${res.status}` };
	} catch (e) {
		return { ok: false, text: (e as Error).message };
	}
}

export function compactNow(path: string): Promise<Result> {
	return withServer(path, async (server) => {
		const r = await query(server, "compact");
		void figures(server, path).catch(() => null);
		return r.ok ? { ok: true, message: `${basename(path)} compacted.` } : { ok: false, message: `The compaction was refused: ${r.text}` };
	});
}

// ------------------------------------------------------------------ figures

/** What `server` says of its database now, remembered for when none runs. */
async function figures(server: FileServer, path: string): Promise<Figures> {
	const res = await fetch(`${server.local}/_metrics`, { headers: { authorization: `Bearer ${server.token}` }, redirect: "manual", signal: AbortSignal.timeout(5000) });
	if (!res.ok) throw new Error(`HTTP ${res.status}`);
	const cur = readMetrics(await res.text(), Date.now());
	const { perSecond, median } = between(samples.get(server) ?? null, cur);
	samples.set(server, cur);
	const seen = { collections: cur.collections, rows: cur.rows, fileBytes: cur.fileBytes, reclaimable: cur.reclaimable, at: cur.at, first: cur.names.find((n) => /^[A-Za-z_]\w*$/.test(n) && !n.startsWith("_")) ?? "" };
	store.updateDatabase(path, (d) => (d.seen = seen));
	return { ...seen, live: true, connections: cur.connections, perSecond, median };
}

/** Read once a moment after a server starts, so the lists have figures for it. */
async function soonFigures(server: FileServer, path: string) {
	await Bun.sleep(1500);
	await figures(server, path).catch(() => null);
	changed();
}

/** The figures of a database: from the server running on it, or as last seen. */
export async function figuresOf(path: string): Promise<Figures | null> {
	const server = served.get(path)?.server ?? windows.get(`file:${path}`)?.server ?? tasks.get(path);
	if (server) {
		try {
			return await figures(server, path);
		} catch {
			// Stopping, or busy: what was seen last.
		}
	}
	const seen = store.database(path)?.seen;
	return seen ? { ...seen, live: false } : null;
}

// ------------------------------------------------------------------ a new database

export type Created = Result & { path?: string; serve?: boolean };

/** A folder that exists and can be written to, as its real path. */
function writableFolder(folder: string): string | null {
	try {
		const dir = realpathSync(folder);
		if (!statSync(dir).isDirectory()) return null;
		accessSync(dir, constants.W_OK);
		return dir;
	} catch {
		return null;
	}
}

/**
 * A database that does not exist yet: `name` in `folder`. The name is refused
 * if the folder already holds a file by it, atomically (`wx`), so a file is
 * never made over. The file is begun empty and a server is started on it for a
 * moment, which makes it a database, and, if one is asked for, its first
 * collection: never the format written here. Then it opens in the studio, or
 * the main window shows the serve sheet on it.
 */
export async function createDatabase(form: Pick<NewDbForm, "folder" | "name"> & Partial<NewDbForm>): Promise<Created> {
	const named = databaseFileName(form.name);
	if (!named.ok) return { ok: false, message: named.message };
	const dir = writableFolder(form.folder);
	if (!dir) return { ok: false, message: "That folder does not exist or cannot be written to. Choose another." };
	const sync = form.sync ?? "250";
	if (form.start === "archive") {
		if (!form.archive) return { ok: false, message: "Choose the folder of backups to start from." };
		const made = await restoreInto(form.archive, null, dir, named.fileName, sync);
		if (!made.ok || !made.path) return made;
		return finish(made.path, form.serve ?? false);
	}
	const statement = form.collection && form.collection.name.trim() !== "" ? collectionStatement(form.collection.name, form.collection.fields) : null;
	if (statement && !statement.ok) return { ok: false, message: statement.message };
	const path = join(dir, named.fileName);
	try {
		closeSync(openSync(path, "wx"));
	} catch (err) {
		if ((err as NodeJS.ErrnoException).code === "EEXIST") {
			return { ok: false, message: `${named.fileName} exists in that folder already. Choose another name, or open it with Open.` };
		}
		return { ok: false, message: `Could not create ${named.fileName}: ${(err as Error).message}` };
	}
	store.updateDatabase(path, (d) => (d.sync = sync));
	let refused = "";
	const made = await withServer(
		path,
		async (server) => {
			if (statement) {
				const r = await query(server, statement.statement);
				// The database is made either way: what the server said goes with it.
				if (!r.ok) refused = r.text;
			}
			return { ok: true };
		},
		{ empty: true },
	);
	if (!made.ok) {
		// The server never took it: do not leave an empty file the user did not ask to keep.
		if (statSync(path, { throwIfNoEntry: false })?.size === 0) {
			unlinkSync(path);
			store.forget(path);
		}
		return made;
	}
	const done = await finish(path, form.serve ?? false);
	return refused ? { ...done, message: `${named.fileName} was made, but its first collection was refused: ${refused}. Make it in the studio.` } : done;
}

/** A new database made: opened in the studio, or handed to the serve sheet. */
async function finish(path: string, serve: boolean): Promise<Created> {
	store.touch(path);
	if (serve) {
		askToServe(path);
		changed();
		return { ok: true, path, serve: true };
	}
	const opened = await openFile(path);
	return { ...opened, path };
}

// ------------------------------------------------------------------ backups

/** Where a database's backups go, sealed with `keyFile` when one is named. */
export function setArchive(path: string, dir: string, keyFile: string | null): Result {
	if (!isAbsolute(dir)) return { ok: false, message: "Choose the folder the backups go to." };
	if (keyFile !== null && !existsSync(keyFile)) return { ok: false, message: `There is no key file at ${keyFile}.` };
	if (keyFile !== null && dirname(keyFile) === dir) return { ok: false, message: "Keep the key apart from the backups: anyone with both can read them. Choose a key file in another folder." };
	store.updateDatabase(path, (d) => (d.archive = { dir, keyFile }));
	changed();
	return { ok: true, message: keyFile ? "Its backups go there, sealed with that key." : "Its backups go there." };
}

const archiveOf = (path: string): Archive | null => {
	const a = store.database(path)?.archive;
	return a ? { dir: a.dir, keyFile: a.keyFile } : null;
};

/** An image of the database, taken while it runs, into its archive folder. */
export async function backupNow(path: string): Promise<Result> {
	const archive = archiveOf(path);
	if (!archive) return { ok: false, message: "Choose where its backups go first." };
	const done = await withServer(path, async (server): Promise<Result> => {
		if (!server.replicationToken) return { ok: false, message: SYNC_OFF_BACKUP };
		const r = await takeBackup(runCli, { local: server.local, replicationToken: server.replicationToken }, archive);
		if (!r.ok) return { ok: false, message: `The backup failed: ${r.message}` };
		store.updateDatabase(path, (d) => {
			if (d.archive) Object.assign(d.archive, { lastAt: Date.now(), lastChange: r.change });
		});
		fact("backup", { path, change: r.change, dir: archive.dir });
		return { ok: true, message: `Backed up ${basename(path)} at change ${r.change}, into ${archive.dir}.` };
	});
	changed();
	return done;
}

export async function verifyBackups(path: string): Promise<Result> {
	const archive = archiveOf(path);
	if (!archive) return { ok: false, message: "It has no backups set up." };
	const r = await verifyArchive(runCli, archive);
	return { ok: r.ok, message: r.text };
}

export const imagesOf = (path: string) => {
	const archive = archiveOf(path);
	return archive ? listImages(archive.dir) : [];
};

/** A backup restored as a new file in `folder`, never over one, and opened. */
async function restoreInto(archive: Archive, change: number | null, folder: string, fileName: string, sync: Sync): Promise<Created> {
	const r = await restoreArchive(runCli, archive, change, join(folder, fileName));
	if (!r.ok) return { ok: false, message: r.message };
	const path = realpathSync(r.path);
	store.updateDatabase(path, (d) => (d.sync = sync));
	fact("restored", { path, change: r.change });
	return { ok: true, path, message: `${fileName} is the database at change ${r.change}.` };
}

/** Restores a database's backup into a new file, `name` in `folder`, and opens it. */
export async function restoreBackup(path: string, change: number | null, folder: string, name: string): Promise<Created> {
	const archive = archiveOf(path);
	if (!archive) return { ok: false, message: "It has no backups set up." };
	const named = databaseFileName(name);
	if (!named.ok) return { ok: false, message: named.message };
	const dir = writableFolder(folder);
	if (!dir) return { ok: false, message: "That folder does not exist or cannot be written to. Choose another." };
	const made = await restoreInto(archive, change, dir, named.fileName, store.database(path)?.sync ?? "250");
	if (!made.ok || !made.path) return made;
	const opened = await openFile(made.path);
	changed();
	return { ...opened, path: made.path, message: made.message };
}

/** A new key file, `name` in `folder`, readable by its owner alone (`fenec key`). */
export async function makeKey(folder: string, name: string): Promise<Result & { path?: string }> {
	const dir = writableFolder(folder);
	if (!dir) return { ok: false, message: "That folder does not exist or cannot be written to. Choose another." };
	if (!/^[^/\\:\u0000-\u001f.][^/\\:\u0000-\u001f]*$/.test(name.trim())) return { ok: false, message: "Name the key file." };
	const path = join(dir, name.trim());
	if (existsSync(path)) return { ok: false, message: `${name.trim()} exists already: a key written over is a backup lost.` };
	const r = await makeKeyFile(runCli, path);
	return { ok: r.ok, message: r.message, ...(r.ok ? { path } : {}) };
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

/** The token saved for `c`, for the address it names now; "" if none. Throws if the keychain did not answer. */
async function tokenOf(c: StoredConnection, origin: string): Promise<string> {
	// The token for this address: an item saved for another is none (token.ts).
	return tokenFor(await secrets.get(keychainName(c.id)), origin);
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
		token = await tokenOf(c, origin);
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

/**
 * "Check now": the same look an open takes (remote.ts), without a window, and
 * how long the server took to answer it. Kept for this run, shown under "Last check".
 */
export async function checkConnection(id: string): Promise<Check> {
	const c = store.connections().find((x) => x.id === id);
	const at = Date.now();
	if (!c) return { at, ok: false, text: "That connection is gone." };
	let check: Check;
	try {
		const origin = originOf(c.url);
		const token = await tokenOf(c, origin);
		const t0 = performance.now();
		const probe = await probeRemote(origin, token, c.tenant);
		const ms = Math.round(performance.now() - t0);
		if (probe.kind === "ok") {
			check = { at, ok: true, text: `Answered in ${ms} ms. Its studio is on${c.tenant ? `; the token reaches tenant ${c.tenant}` : token ? "; the token is accepted" : "; it asks for no token"}.` };
		} else {
			const n = remoteNotice(c.name, origin, probe);
			check = { at, ok: false, text: `${n.message} ${n.hint ?? ""}`.trim() };
		}
	} catch (err) {
		check = { at, ok: false, text: (err as Error).message };
	}
	checks.set(id, check);
	return check;
}

/** Set by the main window's module: shows it with this connection ready to edit. */
let editor: (id: string) => void = () => {};
export const onEdit = (f: (id: string) => void) => (editor = f);
const openEditor = (id: string) => editor(id);

/**
 * Saves a connection. **A token is for the address it was saved with**: a
 * connection pointed at another address with no token typed keeps none, and
 * its saved one is deleted, so it never reaches another server by accident.
 * `forgotToken` says that happened.
 */
export async function saveConnection(form: ConnectionForm): Promise<{ id: string; forgotToken: boolean }> {
	const origin = originOf(form.url);
	const id = form.id ?? crypto.randomUUID();
	const previous = store.connections().find((c) => c.id === id);
	if (form.id && !previous) throw new Error("That connection is gone.");
	const moved = previous !== undefined && addressOf(previous) !== origin;
	// The token goes to the keychain first and nowhere else on disk; a
	// failure there leaves the list as it was.
	if (form.token) await secrets.set(keychainName(id), sealToken(origin, form.token));
	store.saveConnection({ id, name: form.name.trim() || new URL(origin).host, url: origin, tenant: form.tenant.trim() || null });
	// After the address is saved: the item is sealed for the old one, so an open
	// that read it in between sends nothing to the new one (token.ts).
	const forgotToken = moved && !form.token;
	if (forgotToken) await secrets.delete(keychainName(id));
	checks.delete(id);
	changed();
	return { id, forgotToken };
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

export async function removeConnection(id: string) {
	store.removeConnection(id);
	// A window open on it holds its store, and would write to it again: it goes first.
	const open = windows.get(`remote:${id}`);
	if (open) await closeWindow(open);
	await secrets.delete(keychainName(id));
	// What its windows kept goes with it.
	clearStore(id);
	checks.delete(id);
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

// ------------------------------------------------------------------ the main window's view

/** A database out of the list: its recent entry and what was kept for it, its served token too. Not while it runs. */
export async function forgetDatabase(path: string): Promise<Result> {
	if (served.has(path) || windows.has(`file:${path}`) || lanes.has(path)) return { ok: false, message: "Stop its server and close its window first." };
	const d = store.database(path);
	store.forget(path);
	if (d) await secrets.delete(serveItemName(d.id)).catch(() => {});
	changed();
	return { ok: true };
}

let bundled = "";
/** `fenec-server <version>`, asked of the bundled server once. */
async function bundledVersion(): Promise<string> {
	if (bundled) return bundled;
	try {
		const p = Bun.spawn([serverBinary(), "--version"], { stdin: "ignore", stdout: "pipe", stderr: "ignore", env: {} });
		bundled = (await new Response(p.stdout).text()).trim();
	} catch {
		bundled = "fenec-server";
	}
	return bundled;
}

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
	const recents = store.recents();
	const stored = store.databases();
	const paths = [...new Set([...recents.map((r) => r.path), ...stored.map((d) => d.path)])];
	const databases: DatabaseRow[] = paths.map((path) => {
		const d = stored.find((x) => x.path === path) ?? { path, id: "", sync: "250" as Sync };
		const stat = statSync(path, { throwIfNoEntry: false });
		const s = served.get(path);
		const w = windows.get(`file:${path}`);
		return {
			path,
			name: basename(path),
			folder: dirname(path),
			exists: Boolean(stat?.isFile()),
			openedAt: recents.find((r) => r.path === path)?.openedAt ?? null,
			size: stat?.isFile() ? stat.size : null,
			sync: d.sync,
			figures: d.seen ? { ...d.seen, live: false } : null,
			window: Boolean(w),
			running: s ? "served" : w?.server ? "window" : tasks.has(path) ? "task" : "none",
			served: servedRow(d),
			archive: d.archive ? { dir: d.archive.dir, keyFile: d.archive.keyFile, lastAt: d.archive.lastAt ?? null, lastChange: d.archive.lastChange ?? null } : null,
		};
	});
	return {
		databases,
		connections,
		checks: Object.fromEntries(checks),
		open: [...windows.keys()],
		keychain: secrets.kind,
		bundled: await bundledVersion(),
		home: Utils.paths.home,
	};
}
