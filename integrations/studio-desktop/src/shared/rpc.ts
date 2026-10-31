// What the app's own pages may ask the main process. Two pages have an RPC
// bridge, both bundled with the app and trusted: the main window (databases,
// servers, connections, backups: `launcher` in the code, the name it has had
// since it was the launcher) and the new-database window. The notice window is
// sandboxed (it can only report a click), and a studio window -- a page a
// server serves, possibly a remote one -- has no bridge at all.

import type { Image } from "../bun/backups.ts";
import type { ServeSettings, Snippets, Sync } from "../bun/serving.ts";

export type { Image, ServeSettings, Snippets, Sync };

/** What an open or a connect answers. `shown`: the main process put its own message on screen already. */
export type Result = { ok: boolean; message?: string; shown?: boolean };

/** What a server said about its database: live from its `/_metrics`, or as last seen. */
export type Figures = {
	collections: number;
	rows: number;
	fileBytes: number;
	reclaimable: number;
	/** When it was read. */
	at: number;
	/** Read from a server running now, rather than remembered. */
	live: boolean;
	connections?: number;
	/** Statements a second and the median answer in s since the last read; null when nothing was answered. */
	perSecond?: number | null;
	median?: number | null;
};

/** A database the app has served, running or stopped with the settings it last ran with. */
export type ServedRow = {
	state: "running" | "starting" | "stopping" | "stopped";
	settings: ServeSettings;
	/** Where an app on this Mac reaches it, while it runs. */
	url: string | null;
	/** Where another machine reaches it, while it runs on the network. */
	networkUrls: string[];
	metricsUrl: string | null;
	startedAt: number | null;
	lastRan: { at: number; port: number | null; why: string } | null;
	facts: string;
	/** What the app runs, its secrets as dots. */
	command: string;
	snippets: Snippets | null;
};

export type DatabaseRow = {
	path: string;
	name: string;
	folder: string;
	exists: boolean;
	openedAt: number | null;
	/** The file's size on disk. */
	size: number | null;
	sync: Sync;
	figures: Figures | null;
	/** A studio window is open on it. */
	window: boolean;
	/** What runs on it now: nothing, a window's server, its served server, or one taking a backup. */
	running: "none" | "window" | "served" | "task";
	served: ServedRow | null;
	archive: { dir: string; keyFile: string | null; lastAt: number | null; lastChange: number | null } | null;
};

export type Connection = { id: string; name: string; url: string; tenant: string | null; hasToken: boolean };
export type Check = { at: number; ok: boolean; text: string };

export type Overview = {
	databases: DatabaseRow[];
	connections: Connection[];
	/** The last "Check now" of each saved connection, this run. */
	checks: Record<string, Check>;
	open: string[];
	keychain: string;
	/** `fenec-server 0.1.12`, the server the app carries. */
	bundled: string;
	/** The home folder, which a path under it is shown with as ~. */
	home: string;
	/** A saved server the main window should open its form on (a notice's "Edit connection" asked for it), once. */
	edit?: string | null;
	/** A database the main window should open the serve sheet on (New database's "Serve it", a notice's "Change the port"), once. */
	serve?: string | null;
};

export type ConnectionForm = {
	/** Set to change a saved connection; its token stays unless a new one is given or the address changes. */
	id?: string;
	name: string;
	url: string;
	token: string;
	tenant: string;
};

export type LauncherRPC = {
	bun: {
		requests: {
			overview: { params: {}; response: Overview };
			chooseFile: { params: {}; response: Result };
			newDatabase: { params: {}; response: Result };
			openPath: { params: { path: string }; response: Result };
			forget: { params: { path: string }; response: Overview };
			reveal: { params: { path: string }; response: Result };
			figures: { params: { path: string }; response: Figures | null };
			compact: { params: { path: string }; response: Result };

			serve: { params: { path: string; settings: ServeSettings; sync: Sync }; response: Result };
			stopServer: { params: { path: string }; response: Result };
			restartServer: { params: { path: string; settings?: ServeSettings; sync?: Sync }; response: Result };
			removeServer: { params: { path: string }; response: Overview };
			/** What the sheet would run, for settings not yet started. */
			plan: { params: { path: string; settings: ServeSettings; sync: Sync }; response: { command: string; facts: string } };
			portFree: { params: { port: number; reach: "mac" | "lan"; path: string }; response: { free: boolean; text: string } };
			/** The served token, for Show; made and kept if there is none. */
			serveToken: { params: { path: string }; response: { token: string } };
			copyToken: { params: { path: string }; response: Result };
			newToken: { params: { path: string }; response: Result };
			serverLog: { params: { path: string }; response: { lines: string[] } };
			copyText: { params: { text: string }; response: Result };

			chooseFolder: { params: { from: string }; response: { folder: string | null } };
			chooseKey: { params: { from: string }; response: { path: string | null } };
			makeKey: { params: { folder: string; name: string }; response: Result & { path?: string } };
			setArchive: { params: { path: string; dir: string; keyFile: string | null }; response: Result };
			backup: { params: { path: string }; response: Result };
			verify: { params: { path: string }; response: Result };
			images: { params: { path: string }; response: Image[] };
			restore: { params: { path: string; change: number | null; folder: string; name: string }; response: Result & { path?: string } };

			saveConnection: { params: ConnectionForm; response: { ok: boolean; message?: string; overview?: Overview; forgotToken?: boolean } };
			openConnection: { params: { id: string }; response: Result };
			checkConnection: { params: { id: string }; response: Check };
			removeConnection: { params: { id: string }; response: Overview };
			forgetToken: { params: { id: string }; response: Overview };
		};
		messages: {};
	};
	webview: {
		requests: {};
		/** What the main window lists changed (a window opened or closed, a server started or stopped): ask for the overview again. */
		messages: { changed: {} };
	};
};

export type NewDbForm = {
	folder: string;
	name: string;
	sync: Sync;
	/** Empty, or restored from an archive folder of backups. */
	start: "empty" | "archive";
	archive?: { dir: string; keyFile: string | null };
	/** A first collection, written as FenecQL writes one: `notes` and `title text, done bool`. */
	collection?: { name: string; fields: string };
	/** Show the serve sheet for it once it is made, rather than a studio window. */
	serve: boolean;
};

export type NewDbRPC = {
	bun: {
		requests: {
			/** Where a new file goes unless the user picks: the Documents folder. */
			defaults: { params: {}; response: { folder: string; syncNotes: Record<Sync, string> } };
			/** The folder dialog; `null` when it was cancelled. */
			chooseFolder: { params: { from: string }; response: { folder: string | null } };
			chooseKey: { params: { from: string }; response: { path: string | null } };
			create: { params: NewDbForm; response: Result & { path?: string } };
			close: { params: {}; response: {} };
		};
		messages: {};
	};
	webview: {
		requests: {};
		messages: {};
	};
};

/** What a notice window shows, handed in by the window's preload as `window.__notice`. */
export type Notice = {
	title: string;
	message: string;
	/** What to do about it. */
	hint?: string;
	/** What the server or the network said, for whoever reports it. */
	detail?: string;
	/** Buttons besides OK, which always closes. */
	actions?: { id: string; label: string }[];
};
