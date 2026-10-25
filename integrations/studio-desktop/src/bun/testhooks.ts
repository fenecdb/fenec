// What the scripted checks (scripts/checks/) drive the app with. Bundled into
// the test variant only: every use is behind `__TESTABLE__`, which
// electrobun.config.ts sets true for FENEC_DESKTOP_TEST_BUILD=1 and false in
// every other build, and the bundler drops what it guards. A shipped app
// reads none of these variables, runs no command from a file and reports
// nothing (scripts/checks/release.mjs holds the shipped bundle to it).
//
//   FENEC_DESKTOP_REPORT   a file the app appends one JSON line a fact to
//   FENEC_DESKTOP_CONTROL  a file of JSON commands, one a line, read as it grows
//   FENEC_DESKTOP_OPEN     files to open at start, joined by ":"
//   FENEC_DESKTOP_LAUNCHER show the launcher at start
//
// A page reports through the event-only bridge a window keeps even when it is
// sandboxed (`__electrobunSendToHost`), which a server's page cannot reach
// the main process by.

import { appendFileSync, readFileSync } from "node:fs";
import { Utils, type BrowserWindow } from "electrobun/main";
import type { Notice } from "../shared/rpc.ts";
import { createDatabase, onFact, openConnection, openFile, removeConnection, saveConnection, windows } from "./app.ts";
import { showLauncher, showNewDatabase } from "./pages.ts";
import { secrets } from "./secrets.ts";

const T0 = Date.now();
const REPORT = process.env["FENEC_DESKTOP_REPORT"] ?? "";
const CONTROL = process.env["FENEC_DESKTOP_CONTROL"] ?? "";

/** One fact for the check that is watching. */
export function report(what: string, extra: Record<string, unknown> = {}) {
	if (!REPORT) return;
	appendFileSync(REPORT, JSON.stringify({ at: Date.now(), since: Date.now() - T0, what, ...extra }) + "\n");
}

const detailOf = (e: any): any => e?.data?.detail ?? e?.data;

/** Windows by the name a check addresses them: a studio window's key, `launcher`, `newdb`, `notice:<n>`. */
const pages = new Map<string, BrowserWindow>();
let notices = 0;

/** A link out, which a check must not send the machine's browser to. */
export function external(url: string) {
	report("external", { url });
}

/** Reports what the studio in a window shows, until the check stops looking. */
export function pagePreload(origin: string): string {
	const o = JSON.stringify(origin);
	return [
		`(function(){if(location.origin!==${o})return;`,
		`var n=0,last="",t=setInterval(function(){n++;`,
		`var txt=document.body?document.body.innerText:"";`,
		`var signin=/Connect to a fenecdb server/.test(txt);`,
		`var loaded=!signin&&/Rows/.test(txt)&&/Schema/.test(txt)&&/\\d/.test(txt);`,
		`var side=(txt.match(/\\n[a-z_]+\\n\\d[\\d,]*/g)||[]).join("|").replace(/\\n/g," ");`,
		`var sig=signin+"/"+loaded+"/"+side+"/"+document.title;`,
		`if(sig!==last||n===1){last=sig;`,
		`window.__electrobunSendToHost&&window.__electrobunSendToHost({type:"state",n:n,at:Date.now(),signin:signin,loaded:loaded,side:side,title:document.title,text:txt.slice(0,300)});}`,
		`if(n>1200)clearInterval(t);},100);})();`,
	].join("");
}

export function watchStudio(win: BrowserWindow, key: string) {
	pages.set(key, win);
	(win.webview as any).on("host-message", (e: any) => report("page", { key, detail: detailOf(e) }));
	for (const name of ["will-navigate", "did-navigate", "new-window-open"]) {
		(win.webview as any).on(name, (e: any) => report(name, { key, data: JSON.stringify(e?.data ?? e).slice(0, 200) }));
	}
	win.on("close", () => {
		report("window-close", { key });
		pages.delete(key);
	});
}

export function noticeShown(win: BrowserWindow, n: Notice) {
	const key = `notice:${++notices}`;
	pages.set(key, win);
	report("notice", { key, title: n.title, message: n.message, hint: n.hint, detail: n.detail, actions: (n.actions ?? []).map((a) => a.id) });
	// A check presses the notice's buttons by running script in it, and hears back here.
	(win.webview as any).on("host-message", (e: any) => {
		const d = detailOf(e);
		if (d?.type === "eval") report("page", { key, detail: d });
		if (d?.type === "notice-ready") report("notice-ready", { key });
	});
	win.on("close", () => {
		report("notice-closed", { key });
		pages.delete(key);
	});
}

/** The text an app page shows, reported when it changes. */
export function pagePreloadFor(name: string): string {
	return `(function(){var last="";setInterval(function(){var t=(document.body?document.body.innerText:"").replace(/\\s+/g," ").slice(0,900);if(t!==last){last=t;window.__electrobunSendToHost&&window.__electrobunSendToHost({type:"page-text",name:${JSON.stringify(name)},text:t});}},300);})();`;
}

export function watchPage(win: BrowserWindow, name: string) {
	pages.set(name, win);
	(win.webview as any).on("host-message", (e: any) => {
		const d = detailOf(e);
		report(d?.type === "page-text" ? "page-text" : "page", { key: name, detail: d, text: d?.text });
	});
	win.on("close", () => {
		report("window-close", { key: name });
		pages.delete(name);
	});
}

// What a native dialog would have answered, queued by a check: the open and
// folder dialogs cannot be clicked from outside the process.
const answers: { file: (string | null)[]; folder: (string | null)[] } = { file: [], folder: [] };

/** The queued answer for a dialog, or `undefined` for none (the dialog then opens). */
export function dialogAnswer(kind: "file" | "folder"): string | null | undefined {
	return answers[kind].length ? answers[kind].shift() : undefined;
}

async function run(c: any) {
	report("cmd", { cmd: c });
	const win = (key: string) => pages.get(key);
	switch (c.cmd) {
		case "open": report("open-result", { ...(await openFile(c.path)), path: c.path }); break;
		case "create": report("create-result", { ...(await createDatabase(c.folder, c.name)), name: c.name }); break;
		case "close": win(c.key)?.requestClose(); break; // the path the red button takes
		case "close-direct": win(c.key)?.close(); break;
		// executeJavascript is not the page's own eval(): the studio's CSP
		// (script-src 'self') does not apply to it. The code reports its
		// result itself, through the event-only bridge.
		case "eval": win(c.key)?.webview.executeJavascript(c.js); break;
		case "sessions": report("sessions", { keys: [...windows.keys()], pids: [...windows.values()].map((s) => s.server?.pid), pages: [...pages.keys()] }); break;
		case "answer": answers[c.kind as "file" | "folder"].push(c.path ?? null); break;
		case "launcher": await showLauncher(); break;
		case "newdb": await showNewDatabase(); break;
		case "addconn": {
			try {
				report("conn-added", { id: await saveConnection(c), name: c.name });
			} catch (e) {
				report("conn-refused", { message: (e as Error).message });
			}
			break;
		}
		case "openconn": report("openconn-result", { ...(await openConnection(c.id)), id: c.id }); break;
		case "rmconn": await removeConnection(c.id); report("conn-removed", { id: c.id }); break;
		case "secret": report("secret", { name: c.name, value: await secrets.get(c.name) }); break;
		case "quit": Utils.quit(); break;
	}
}

/** Starts what the environment asks for. Returns whether it opened something the launcher need not cover. */
export async function boot(): Promise<boolean> {
	onFact(report);
	if (CONTROL) {
		let offset = 0;
		setInterval(async () => {
			let text = "";
			try {
				text = readFileSync(CONTROL, "utf8");
			} catch {
				return;
			}
			const fresh = text.slice(offset);
			const end = fresh.lastIndexOf("\n") + 1;
			offset += end;
			for (const line of fresh.slice(0, end).split("\n").filter(Boolean)) await run(JSON.parse(line));
		}, 150);
	}
	report("ready");
	const open = process.env["FENEC_DESKTOP_OPEN"];
	if (open) for (const p of open.split(":")) await openFile(p);
	if (process.env["FENEC_DESKTOP_LAUNCHER"]) await showLauncher();
	return Boolean(open) || Boolean(process.env["FENEC_DESKTOP_LAUNCHER"]);
}
