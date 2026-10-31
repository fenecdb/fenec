// How a studio window learns who it is: the one place a token reaches a page.
//
// An app embedding the studio hands it its session as `window.fenecHost`
// (site/content/docs/studio.html#host, studio/connect.js `saved()`): frozen,
// defined by the window's preload -- a user script that runs at document start
// in the page's own world, before any of its modules -- and naming the server's
// origin. The studio takes it only if it is frozen and names the origin that
// served the page; it never writes the token to storage, shows no sign-in form
// and no Sign out (the app holds the connection), and says so if the app
// handed it nothing. The token is never in a URL, a query, a fragment, argv,
// the page's HTML or its storage.
//
// Two locks keep it from reaching a page that is not the server's:
//   1. the preload defines it only where `location.origin` is the server's own
//      (below): it runs in every page the window loads, a redirect's or a
//      sign-in page's, and a page of another origin that found `fenecHost`
//      would read the token (the studio cannot hide it from one);
//   2. the window's navigation rules do not let it leave that origin at all
//      (windows.ts).

export type StudioSession = {
	/** The server's origin, `http://127.0.0.1:52341`. The window's page is `<origin>/_studio/`. */
	origin: string;
	token: string;
	/** A tenant node's tenant, for a saved connection that names one. */
	tenant: string | null;
};

/**
 * The preload script that hands `session` to the page the window opens.
 *
 * Every value goes in as a JSON literal, so a token is data in the script and
 * never code. The property is not writable, enumerable or configurable, and
 * the object and its session are frozen, as the studio asks of them.
 */
export function sessionPreload(session: StudioSession): string {
	const origin = JSON.stringify(session.origin);
	const value = JSON.stringify({ server: session.origin, token: session.token, tenant: session.tenant });
	return (
		`(function(){try{if(location.origin!==${origin})return;` +
		`Object.defineProperty(window,"fenecHost",{value:Object.freeze({session:Object.freeze(${value})})});` +
		`}catch(e){}})();`
	);
}
