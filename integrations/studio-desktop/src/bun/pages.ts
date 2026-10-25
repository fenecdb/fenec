// The app's own pages: the launcher (recent files, saved servers) and the
// new-database window. Both are bundled with the app, trusted, and get the
// RPC bridge; what they may ask is `shared/rpc.ts`, and what answers is
// app.ts. Neither page shows anything a server served.

import { homedir } from "node:os";
import { BrowserView, BrowserWindow, Utils } from "electrobun/main";
import type { LauncherRPC, NewDbRPC } from "../shared/rpc.ts";
import { createDatabase, onChange, onEdit, openConnection, openFile, overview, removeConnection, saveConnection } from "./app.ts";
import { store } from "./store.ts";
import { pageFrame } from "./windows.ts";

/** The dialogs. A test variant answers from the checks (testhooks.ts) instead of opening one. */
async function pick(kind: "file" | "folder", startingFolder: string): Promise<string | null> {
	if (__TESTABLE__) {
		const answer = (await import("./testhooks.ts")).dialogAnswer(kind);
		if (answer !== undefined) return answer;
	}
	const paths = await Utils.openFileDialog({
		startingFolder,
		canChooseFiles: kind === "file",
		canChooseDirectory: kind === "folder",
		allowsMultipleSelection: false,
	});
	return paths.find((p) => p) ?? null;
}

export async function chooseFile() {
	const path = await pick("file", homedir());
	return path ? openFile(path) : { ok: true };
}

// ------------------------------------------------------------------ launcher

let launcher: BrowserWindow | null = null;
/** A saved server the launcher is to open with its form filled in, asked for by a notice's "Edit connection". */
let pendingEdit: string | null = null;

const launcherRPC = BrowserView.defineRPC<LauncherRPC>({
	maxRequestTime: 60_000,
	handlers: {
		requests: {
			overview: async () => {
				const o = await overview();
				const edit = pendingEdit;
				pendingEdit = null;
				return { ...o, edit };
			},
			chooseFile: () => chooseFile(),
			newDatabase: async () => {
				await showNewDatabase();
				return { ok: true };
			},
			openPath: ({ path }) => openFile(path),
			forgetRecent: async ({ path }) => {
				store.forgetRecent(path);
				return overview();
			},
			saveConnection: async (form) => {
				try {
					await saveConnection(form);
					return { ok: true, overview: await overview() };
				} catch (err) {
					return { ok: false, message: (err as Error).message };
				}
			},
			openConnection: ({ id }) => openConnection(id),
			removeConnection: async ({ id }) => {
				await removeConnection(id);
				return overview();
			},
		},
		messages: {},
	},
});

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
		frame: pageFrame(760, 680),
		preload: hooks?.pagePreloadFor("launcher"),
	});
	hooks?.watchPage(launcher, "launcher");
	launcher.on("close", () => {
		launcher = null;
	});
}

onEdit((id) => {
	pendingEdit = id;
	if (launcher) {
		launcher.activate();
		void launcherRPC.send.changed({});
	} else void showLauncher();
});

// What the launcher lists changes as windows open and close and files are used.
onChange(() => {
	if (launcher) void launcherRPC.send.changed({});
});

// ------------------------------------------------------------------ new database

let newdb: BrowserWindow | null = null;

const newDbRPC = BrowserView.defineRPC<NewDbRPC>({
	maxRequestTime: 120_000,
	handlers: {
		requests: {
			defaults: () => ({ folder: Utils.paths.documents }),
			chooseFolder: async ({ from }) => ({ folder: await pick("folder", from || Utils.paths.documents) }),
			create: async ({ folder, name }) => {
				const made = await createDatabase(folder, name);
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
		frame: pageFrame(560, 390),
		preload: hooks?.pagePreloadFor("newdb"),
	});
	hooks?.watchPage(newdb, "newdb");
	newdb.on("close", () => {
		newdb = null;
	});
}
