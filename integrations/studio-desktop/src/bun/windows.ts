// The windows a page of someone else's can be in, and the app's own notice.
//
// A studio window shows a page a *server* serves (the one this app started,
// or a saved remote one). The studio is "a page with no authority of its own":
// it holds a token and sends it. So its window:
//   - is `sandbox: true`: no Electrobun RPC to the main process at all;
//   - is held to the server's origin by navigation rules, and a link out
//     (`window.open`, target=_blank) goes to the default browser instead;
//   - learns its token from its preload, never from a URL (session.ts).

import { BrowserWindow, Utils } from "electrobun/main";
import type { Notice } from "../shared/rpc.ts";
import { sessionPreload, type StudioSession } from "./session.ts";

let cascade = 0;

function frame(width: number, height: number) {
	const o = 40 * (cascade++ % 8);
	return { x: 140 + o, y: 90 + o, width, height };
}

/** What a webview event carries, whichever way this Electrobun wraps it. */
const detailOf = (e: any): any => e?.data?.detail ?? e?.data;

/** A link out of a studio window goes to the default browser, and only if it is a web address: never a file: or a custom scheme. */
async function openExternal(url: string) {
	if (!/^https?:/.test(url)) return;
	if (__TESTABLE__) return void (await import("./testhooks.ts")).external(url);
	Utils.openExternal(url);
}

/** A window on `<origin>/_studio/`, signed in as `session` (a server's token, never a URL). */
export async function openStudioWindow(opts: { key: string; title: string; session: StudioSession }): Promise<BrowserWindow> {
	const { origin } = opts.session;
	// A hook that reports what the page shows is a preload too, and only the test variant has it.
	const extra = __TESTABLE__ ? (await import("./testhooks.ts")).pagePreload(origin) : "";
	const win = new BrowserWindow({
		title: opts.title,
		url: `${origin}/_studio/`,
		frame: frame(1280, 820),
		preload: sessionPreload(opts.session) + extra,
		sandbox: true,
		// Stay on the server's origin; everything else goes to the browser.
		navigationRules: JSON.stringify(["^*", `${origin}/*`]),
	});
	// The constructor's `navigationRules` did not take in Electrobun 2.0.4 (a
	// navigation to https://example.com reported "allowed": true with it set);
	// the setter does.
	win.webview.setNavigationRules(["^*", `${origin}/*`]);
	(win.webview as any).on("new-window-open", (e: any) => {
		const d = detailOf(e);
		const url = typeof d === "string" ? d : d?.url;
		if (typeof url === "string") void openExternal(url);
	});
	if (__TESTABLE__) (await import("./testhooks.ts")).watchStudio(win, opts.key);
	return win;
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
