// What the launcher page may ask the main process. The launcher is the one
// page of the app that is bundled and trusted; a studio window (a page served
// by a fenec-server, possibly a remote one) is created with `sandbox: true`
// and has no RPC at all.

export type Recent = { path: string; name: string; openedAt: number; exists: boolean };
export type Connection = { id: string; name: string; url: string; tenant: string | null; hasToken: boolean };
export type Overview = { recents: Recent[]; connections: Connection[]; open: string[]; keychain: string };

export type LauncherRPC = {
	bun: {
		requests: {
			overview: { params: {}; response: Overview };
			chooseFile: { params: {}; response: { ok: boolean; message?: string } };
			openPath: { params: { path: string }; response: { ok: boolean; message?: string } };
			forgetRecent: { params: { path: string }; response: Overview };
			saveConnection: {
				params: { name: string; url: string; token: string; tenant: string };
				response: { ok: boolean; message?: string; overview?: Overview };
			};
			openConnection: { params: { id: string }; response: { ok: boolean; message?: string } };
			removeConnection: { params: { id: string }; response: Overview };
		};
		messages: {};
	};
	webview: {
		requests: {};
		messages: {};
	};
};
