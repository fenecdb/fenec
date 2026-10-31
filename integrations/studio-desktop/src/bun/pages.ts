// The app's own pages: the main window (databases, servers, connections,
// backups; `launcher` in the code) and the new-database window. Both are
// bundled with the app, trusted, and get the RPC bridge; what they may ask is
// `shared/rpc.ts`, and what answers is app.ts. Neither page shows anything a
// server served. A served database's token reaches the main window only when
// it is asked to Show it; the saved servers' tokens never do.

import { homedir } from "node:os";
import { BrowserView, BrowserWindow, Utils } from "electrobun/main";
import type { LauncherRPC, NewDbRPC } from "../shared/rpc.ts";
import {
	backupNow,
	checkConnection,
	compactNow,
	createDatabase,
	figuresOf,
	forgetDatabase,
	forgetToken,
	imagesOf,
	makeKey,
	newServeToken,
	onChange,
	onEdit,
	onServeSheet,
	openConnection,
	openFile,
	overview,
	portFree,
	removeConnection,
	removeServed,
	restartServed,
	restoreBackup,
	saveConnection,
	serveFile,
	serverLog,
	serveTokenOf,
	setArchive,
	sheetPlan,
	stopServed,
	verifyBackups,
} from "./app.ts";
import { copySecret, SECRET_ON_CLIPBOARD_MS, type Clipboard } from "./clipboard.ts";
import { DEFAULT_SYNC, isSync, settingsFrom, SYNC_NOTES, type Sync } from "./serving.ts";
import { holdTo, pageFrame } from "./windows.ts";

/** The dialogs. A test variant answers from the checks (testhooks.ts) instead of opening one. */
async function pick(kind: "file" | "folder" | "key", startingFolder: string): Promise<string | null> {
	if (__TESTABLE__) {
		const answer = (await import("./testhooks.ts")).dialogAnswer(kind);
		if (answer !== undefined) return answer;
	}
	const paths = await Utils.openFileDialog({
		startingFolder,
		// A database is a .fenec file (a comma-separated list of extensions, which
		// the native panel takes as it is); a folder and a key file are any.
		allowedFileTypes: kind === "file" ? "fenec" : "*",
		canChooseFiles: kind !== "folder",
		canChooseDirectory: kind === "folder",
		allowsMultipleSelection: false,
	});
	return paths.find((p) => p) ?? null;
}

export async function chooseFile() {
	const path = await pick("file", homedir());
	return path ? openFile(path) : { ok: true };
}

/** The clipboard, from the main process: a page's `navigator.clipboard` needs a secure context views:// is not. */
const clipboard: Clipboard = {
	write: (text) => Utils.clipboardWriteText(text),
	read: () => Utils.clipboardReadText(),
	clear: () => Utils.clipboardClear(),
};

/** Text that is no secret (a snippet) to the clipboard. */
function copy(text: string) {
	clipboard.write(text);
	if (__TESTABLE__) void import("./testhooks.ts").then((h) => h.report("copied", { length: text.length }));
}

const syncFrom = (v: unknown): Sync => (isSync(v) ? v : DEFAULT_SYNC);

// ------------------------------------------------------------------ the main window

let launcher: BrowserWindow | null = null;
/** A saved server the main window is to open with its form filled in, asked for by a notice's "Edit connection". */
let pendingEdit: string | null = null;
/** A database the main window is to open the serve sheet on. */
let pendingServe: string | null = null;

const attempt = async <T>(f: () => Promise<T>): Promise<T | { ok: false; message: string }> => {
	try {
		return await f();
	} catch (err) {
		return { ok: false, message: (err as Error).message };
	}
};

const launcherRPC = BrowserView.defineRPC<LauncherRPC>({
	maxRequestTime: 35 * 60_000,
	handlers: {
		requests: {
			overview: async () => {
				const o = await overview();
				const edit = pendingEdit;
				const serve = pendingServe;
				pendingEdit = null;
				pendingServe = null;
				return { ...o, edit, serve };
			},
			chooseFile: () => chooseFile(),
			newDatabase: async () => {
				await showNewDatabase();
				return { ok: true };
			},
			openPath: ({ path }) => openFile(path),
			forget: async ({ path }) => {
				await forgetDatabase(path);
				return overview();
			},
			reveal: async ({ path }) => {
				Utils.showItemInFolder(path);
				return { ok: true };
			},
			figures: ({ path }) => figuresOf(path),
			compact: ({ path }) => compactNow(path),

			// What a page sends is held to what a setting is (serving.ts), whatever the page is.
			serve: ({ path, settings, sync }) => serveFile(path, settingsFrom(settings), syncFrom(sync)),
			stopServer: ({ path }) => stopServed(path),
			restartServer: ({ path, settings, sync }) => restartServed(path, settings === undefined ? undefined : settingsFrom(settings), sync === undefined ? undefined : syncFrom(sync)),
			removeServer: async ({ path }) => {
				await removeServed(path);
				return overview();
			},
			plan: async ({ path, settings, sync }) => sheetPlan(path, settingsFrom(settings), syncFrom(sync)),
			portFree: ({ port, reach, path }) => portFree(port, reach, path),
			serveToken: async ({ path }) => ({ token: await serveTokenOf(path) }),
			copyToken: ({ path }) =>
				attempt(async () => {
					copySecret(await serveTokenOf(path), clipboard, SECRET_ON_CLIPBOARD_MS);
					if (__TESTABLE__) void import("./testhooks.ts").then((h) => h.report("copied", { secret: true }));
					return { ok: true, message: "The token is on the clipboard for a minute: it is cleared then if it is still there." };
				}),
			newToken: ({ path }) => attempt(() => newServeToken(path)),
			serverLog: async ({ path }) => ({ lines: serverLog(path) }),
			copyText: async ({ text }) => {
				copy(text);
				return { ok: true, message: "Copied." };
			},

			chooseFolder: async ({ from }) => ({ folder: await pick("folder", from || Utils.paths.documents) }),
			chooseKey: async ({ from }) => ({ path: await pick("key", from || homedir()) }),
			makeKey: ({ folder, name }) => makeKey(folder, name),
			setArchive: async ({ path, dir, keyFile }) => setArchive(path, dir, keyFile),
			backup: ({ path }) => backupNow(path),
			verify: ({ path }) => verifyBackups(path),
			images: async ({ path }) => imagesOf(path),
			restore: ({ path, change, folder, name }) => restoreBackup(path, change, folder, name),

			saveConnection: async (form) => {
				try {
					const { forgotToken } = await saveConnection(form);
					return { ok: true, overview: await overview(), forgotToken };
				} catch (err) {
					return { ok: false, message: (err as Error).message };
				}
			},
			openConnection: ({ id }) => openConnection(id),
			checkConnection: ({ id }) => checkConnection(id),
			removeConnection: async ({ id }) => {
				await removeConnection(id);
				return overview();
			},
			forgetToken: async ({ id }) => {
				await forgetToken(id);
				return overview();
			},
		},
		messages: {},
	},
});

/** Is the main window there? */
export const launcherOpen = () => launcher !== null;

/** Closes the main window, if it is open. */
export function closeLauncher() {
	launcher?.close();
}

export async function showLauncher() {
	if (launcher) {
		launcher.activate();
		return;
	}
	const hooks = __TESTABLE__ ? await import("./testhooks.ts") : null;
	launcher = new BrowserWindow({
		title: "fenec studio",
		url: "views://launcher/index.html",
		rpc: launcherRPC,
		frame: pageFrame(1280, 800),
		preload: hooks?.pagePreloadFor("launcher"),
	});
	holdTo(launcher.webview, "views://launcher/*");
	hooks?.watchPage(launcher, "launcher");
	launcher.on("close", () => {
		launcher = null;
	});
}

/** Shows the main window, and has it ask for what changed (and what it is to open). */
function bringUp() {
	if (launcher) {
		launcher.activate();
		void launcherRPC.send.changed({});
	} else void showLauncher();
}

onEdit((id) => {
	pendingEdit = id;
	bringUp();
});

onServeSheet((path) => {
	pendingServe = path;
	bringUp();
});

// What the main window lists changes as windows open and close, servers start and stop, and files are used.
onChange(() => {
	if (launcher) void launcherRPC.send.changed({});
});

// ------------------------------------------------------------------ new database

let newdb: BrowserWindow | null = null;

const newDbRPC = BrowserView.defineRPC<NewDbRPC>({
	maxRequestTime: 35 * 60_000,
	handlers: {
		requests: {
			defaults: () => ({ folder: Utils.paths.documents, syncNotes: SYNC_NOTES }),
			chooseFolder: async ({ from }) => ({ folder: await pick("folder", from || Utils.paths.documents) }),
			chooseKey: async ({ from }) => ({ path: await pick("key", from || homedir()) }),
			create: async (form) => {
				// The page's `sync` held to what one is, as the main window's are.
				const made = await createDatabase({ ...form, sync: syncFrom(form.sync) });
				// After the answer is on its way: the window is the one asking.
				if (made.ok) setTimeout(() => newdb?.close(), 0);
				return made;
			},
			close: () => {
				setTimeout(() => newdb?.close(), 0);
				return {};
			},
		},
		messages: {},
	},
});

export async function showNewDatabase() {
	if (newdb) {
		newdb.activate();
		return;
	}
	const hooks = __TESTABLE__ ? await import("./testhooks.ts") : null;
	newdb = new BrowserWindow({
		title: "New database",
		url: "views://newdb/index.html",
		rpc: newDbRPC,
		frame: pageFrame(600, 720),
		preload: hooks?.pagePreloadFor("newdb"),
	});
	holdTo(newdb.webview, "views://newdb/*");
	hooks?.watchPage(newdb, "newdb");
	newdb.on("close", () => {
		newdb = null;
	});
}
