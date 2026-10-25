// How a studio window learns who it is: the one place a token reaches a page.
//
// The studio reads `{server, token, tenant}` from its tab's sessionStorage
// under "fenec-studio" (studio/connect.js `saved()`), so the window's
// preload, a user script that runs at document start in the page's own
// context before any of its modules, writes it there. The token is never in a
// URL, a query, a fragment, argv or the page's HTML.
//
// Two locks keep it from reaching a page that is not the server's:
//   1. the preload writes only where `location.origin` is the server's own
//      (below), so a page the window navigated to is handed nothing;
//   2. the window's navigation rules do not let it leave that origin at all
//      (windows.ts).
//
// When studio/connect.js reads a frozen `window.fenecHost.session` first, this
// is the function that changes: the preload defines that property instead of
// writing storage, and nothing else in the app does.

export type StudioSession = {
	/** The server's origin, `http://127.0.0.1:52341`. The window's page is `<origin>/_studio/`. */
	origin: string;
	token: string;
	/** A tenant node's tenant, for a saved connection that names one. */
	tenant: string | null;
};

/** The preload script that hands `session` to the page the window opens. */
export function sessionPreload(session: StudioSession): string {
	const origin = JSON.stringify(session.origin);
	const value = JSON.stringify(JSON.stringify({ server: session.origin, token: session.token, tenant: session.tenant }));
	return `(function(){try{if(location.origin!==${origin})return;sessionStorage.setItem("fenec-studio",${value});}catch(e){}})();`;
}
