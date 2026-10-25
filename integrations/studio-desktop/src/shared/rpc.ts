// What the app's own pages may ask the main process. Two pages have an RPC
// bridge, both bundled with the app and trusted: the launcher and the
// new-database window. The notice window is sandboxed (it can only report a
// click), and a studio window -- a page a server serves, possibly a remote
// one -- has no bridge at all.

/** What an open or a connect answers. `shown`: the main process put its own message on screen already. */
export type Result = { ok: boolean; message?: string; shown?: boolean };

export type Recent = { path: string; name: string; openedAt: number; exists: boolean };
export type Connection = { id: string; name: string; url: string; tenant: string | null; hasToken: boolean };
export type Overview = {
	recents: Recent[];
	connections: Connection[];
	open: string[];
	keychain: string;
	/** A saved server the launcher should open its form on (a notice's "Edit connection" asked for it), once. */
	edit?: string | null;
};

export type ConnectionForm = {
	/** Set to change a saved connection; its token stays unless a new one is given. */
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
			forgetRecent: { params: { path: string }; response: Overview };
			saveConnection: { params: ConnectionForm; response: { ok: boolean; message?: string; overview?: Overview } };
			openConnection: { params: { id: string }; response: Result };
			removeConnection: { params: { id: string }; response: Overview };
			forgetToken: { params: { id: string }; response: Overview };
		};
		messages: {};
	};
	webview: {
		requests: {};
		/** What the launcher lists changed (a window opened or closed): ask for the overview again. */
		messages: { changed: {} };
	};
};

export type NewDbRPC = {
	bun: {
		requests: {
			/** Where a new file goes unless the user picks: the Documents folder. */
			defaults: { params: {}; response: { folder: string } };
			/** The folder dialog; `null` when it was cancelled. */
			chooseFolder: { params: { from: string }; response: { folder: string | null } };
			create: { params: { folder: string; name: string }; response: Result & { path?: string } };
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
