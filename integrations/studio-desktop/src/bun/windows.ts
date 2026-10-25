// The windows a page of someone else's can be in, and the app's own notice.
//
// A studio window shows a page a *server* serves (the one this app started,
// or a saved remote one). The studio is "a page with no authority of its own":
// it holds a token and sends it. So its window:
//   - is `sandbox: true`: no Electrobun RPC to the main process at all;
//   - is held to the server's origin by navigation rules, and a link out
//     (`window.open`, target=_blank) goes to the default browser instead;
//   - learns its token from its preload, never from a URL (session.ts).

import { BrowserView, BrowserWindow, Utils } from "electrobun/main";
import type { Notice } from "../shared/rpc.ts";
import { sessionPreload, type StudioSession } from "./session.ts";

let cascade = 0;

/**
 * A window held to one address: every navigation anywhere else is refused
 * (`^*` denies all, the second rule allows `pattern`, a glob). The whole of
 * the defence for a window that has the RPC bridge, the launcher and the
 * new-database window, which Electrobun's full preload adds to *every*
 * document the window loads: a link or a file dropped on it is a navigation,
 * and what it loaded would be handed `openPath`, `openConnection` and the
 * saved servers. So no window of the app is without these (`checks/pages.mjs`
 * holds each one).
 *
 * The constructor's `navigationRules` option did not take in Electrobun 2.0.4
 * (a navigation to https://example.com reported "allowed": true with it set);
 * the setter does.
 */
export function holdTo(view: BrowserView, pattern: string) {
	view.setNavigationRules(["^*", pattern]);
}

function frame(width: number, height: number) {
	const o = 40 * (cascade++ % 8);
	return { x: 140 + o, y: 90 + o, width, height };
}

/** What a webview event carries, whichever way this Electrobun wraps it. */
const detailOf = (e: any): any => e?.data?.detail ?? e?.data;

/**
 * A link out of a studio window goes to the default browser, and only if it is
 * a web address: never a file: or a custom scheme. Electrobun 2.0.4 does not
 * say whether a click asked for it (a script's `window.open` is the same event),
 * so a window is given at most one in `EXTERNAL_GAP_MS`: a page that asks over and
 * over does not open tabs without end.
 */
const EXTERNAL_GAP_MS = 3000;
async function openExternal(url: string, last: { at: number }) {
	if (!/^https?:/.test(url)) return;
	const now = Date.now();
	if (now - last.at < EXTERNAL_GAP_MS) return;
	last.at = now;
	if (__TESTABLE__) return void (await import("./testhooks.ts")).external(url);
	Utils.openExternal(url);
}

/** A studio window: Electrobun's window, and the view that shows the server's page in it. */
export type StudioWindow = { win: BrowserWindow; view: BrowserView };

/**
 * A window on `<origin>/_studio/`, signed in as `session` (a server's token,
 * never a URL), in the data store `partition` names.
 *
 * Everything a page stores (localStorage, IndexedDB: the studio's query
 * history, its saved queries and its theme) lives in a WebKit data store, and
 * Electrobun's window makes its page's view in the one shared store, keyed by
 * origin alone: a file's server listens on a port the system picks, so a later
 * window on a port that was used before would show the earlier file's history.
 * So the window is made with no page, and the view is made in it with a store
 * of its own: `persist:remote-<id>` for a saved server (kept between runs, and
 * apart from every other saved server's), a name without `persist:` for a file
 * (a store that exists only for this window). The window's own view is removed
 * first, which costs nothing: with it kept, idle memory was 20 MB more.
 */
export async function openStudioWindow(opts: { key: string; title: string; session: StudioSession; partition: string }): Promise<StudioWindow> {
	const { origin } = opts.session;
	// The origin is the pattern the window is held to, where `*` is a wildcard.
	if (/[*?]/.test(origin)) throw new Error(`not a server origin: ${origin}`);
	// A hook that reports what the page shows is a preload too, and only the test variant has it.
	const extra = __TESTABLE__ ? (await import("./testhooks.ts")).pagePreload(origin) : "";
	const f = frame(1280, 820);
	const win = new BrowserWindow({ title: opts.title, url: null, html: null, frame: f, sandbox: true, allowedProtocols: { views: false, appData: false } });
	// The window's own view, blank and in the shared store, is not wanted under
	// the page's: it would be a second web view per window for nothing, and the
	// shared store is the one thing this is made to avoid.
	win.webview?.remove();
	const view = new BrowserView({
		url: `${origin}/_studio/`,
		preload: sessionPreload(opts.session) + extra,
		sandbox: true,
		partition: opts.partition,
		windowId: win.id,
		frame: { x: 0, y: 0, width: f.width, height: f.height },
		autoResize: true,
		// A page a server serves is no reason to answer for views://, whose handler
		// sends `Access-Control-Allow-Origin: *`: it could read the app's own pages.
		allowedProtocols: { views: false, appData: false },
	});
	// Stay on the server's origin; everything else goes to the browser.
	holdTo(view, `${origin}/*`);
	const last = { at: 0 };
	(view as any).on("new-window-open", (e: any) => {
		const d = detailOf(e);
		const url = typeof d === "string" ? d : d?.url;
		if (typeof url === "string") void openExternal(url, last);
	});
	const studio = { win, view };
	if (__TESTABLE__) (await import("./testhooks.ts")).watchStudio(studio, opts.key);
	return studio;
}

/** The notice window's preload: the notice, as data, for the page to draw with `textContent`. */
function noticePreload(n: Notice): string {
	return `window.__notice=${JSON.stringify(n)};`;
}

/**
 * A message in a window of its own. Not Utils.showMessageBox: that is a
 * synchronous call into the native layer (`returns: int`) inside an `async`
 * function, so the main process's JavaScript thread -- every server's
 * supervision, a SIGTERM handler, the launcher's RPC -- stands still until
 * someone clicks OK. A check that never clicked held the whole app for good.
 *
 * The page is bundled (views/notice) and sandboxed, so it has no RPC; its
 * buttons report themselves through the event-only bridge a sandboxed view
 * keeps. OK closes; any other button closes and calls `onAction`.
 */
export async function showNotice(n: Notice, onAction: (id: string) => void = () => {}): Promise<BrowserWindow> {
	const win = new BrowserWindow({
		title: n.title,
		url: "views://notice/index.html",
		preload: noticePreload(n),
		sandbox: true,
		frame: frame(540, 330),
	});
	holdTo(win.webview, "views://notice/*");
	(win.webview as any).on("host-message", (e: any) => {
		const d = detailOf(e);
		if (d?.type !== "notice") return;
		win.close();
		if (typeof d.id === "string" && d.id !== "ok") onAction(d.id);
	});
	if (__TESTABLE__) (await import("./testhooks.ts")).noticeShown(win, n);
	return win;
}

/** A window of the app's own, bundled, with an RPC bridge: the launcher and the new-database window. */
export function pageFrame(width: number, height: number) {
	return frame(width, height);
}
