// fenec studio, desktop. The main process (Bun).
//
// A window per open thing:
//   - a file: a fenec-server this process started for it, and a window on
//     that server's own /_studio/ (so the studio is the one the server was built with);
//   - a remote server: a window on that server's /_studio/, same origin, so no CORS.
// plus a launcher (recent files, saved connections), the one bundled page here.
//
// The studio is "a page with no authority of its own": it holds a token and
// sends it. So its windows are `sandbox: true` (no Electrobun RPC to this
// process), limited by navigation rules to their own origin, and the token is
// handed in by a preload script that writes the studio's own sessionStorage
// key before the page's modules run -- never in a URL.
//
// Run it (a spike; see the pull request for what it found):
//   scripts/get-server.sh                   # bin/fenec-server, built from this checkout (needs --studio)
//   hutch run install && hutch electrobun dev   # `hutch electrobun build --env=stable` makes the .app and .dmg
//   bun scripts/mkdata.mjs /tmp/x.fenec     # a 100 000-row file to open
//   bun scripts/measure.mjs "build/stable-macos-arm64/fenec studio.app" /tmp/x.fenec
// FENEC_SERVER_BIN overrides the bundled server; FENEC_DESKTOP_REPORT / _CONTROL / _OPEN are test
// hooks (a report file, a command file, files to open at start) that a shipped app never sets.

import { appendFileSync, existsSync, readFileSync, realpathSync, statSync } from "node:fs";
import { basename } from "node:path";
import { homedir } from "node:os";
import Electrobun, { ApplicationMenu, BrowserView, BrowserWindow, Utils } from "electrobun/main";
import type { LauncherRPC, Overview } from "../shared/rpc";
import { AlreadyOpen, reapStrays, startFileServer, type FileServer } from "./server";
import { secrets } from "./secrets";
import { store } from "./store";

const T0 = Date.now();
const TEST = process.env["FENEC_DESKTOP_REPORT"] ?? ""; // a file the tests read; unset in real use
const report = (what: string, extra: Record<string, unknown> = {}) => {
	if (!TEST) return;
	const line = JSON.stringify({ at: Date.now(), since: Date.now() - T0, what, ...extra });
	appendFileSync(TEST, line + "\n");
};

// ------------------------------------------------------------------ sessions

type Session = {
	key: string;
	title: string;
	win: BrowserWindow;
	server?: FileServer;
	closing?: Promise<unknown>;
};
const sessions = new Map<string, Session>();
let launcher: BrowserWindow | null = null;
let quitting = false;
let cascade = 0;

/** What a studio page needs to find its token, written where the studio looks (studio/connect.js). */
function handoffScript(origin: string, token: string, tenant: string | null): string {
	const value = JSON.stringify({ server: origin, token, tenant });
	// The origin test is what keeps the token from being written into a
	// page the window navigated to; the navigation rules are the second lock.
	const lines = [
		`(function(){try{`,
		`if(location.origin!==${JSON.stringify(origin)})return;`,
		`sessionStorage.setItem("fenec-studio",${JSON.stringify(value)});`,
		`}catch(e){}})();`,
	];
	if (TEST) {
		// Test hook only: report what the studio shows, through the event-only
		// bridge that sandboxed views keep.
		lines.push(
			`(function(){if(location.origin!==${JSON.stringify(origin)})return;`,
			`var n=0,last="",t=setInterval(function(){n++;`,
			`var txt=document.body?document.body.innerText:"";`,
			`var signin=/Connect to a fenecdb server/.test(txt);`,
			`var loaded=!signin&&/Rows/.test(txt)&&/Schema/.test(txt)&&/\\d/.test(txt);`,
			`var side=(txt.match(/\\n[a-z_]+\\n\\d[\\d,]*/g)||[]).join("|").replace(/\\n/g," ");`,
			`var sig=signin+"/"+loaded+"/"+side+"/"+document.title;`,
			`if(sig!==last||n===1){last=sig;`,
			`window.__electrobunSendToHost&&window.__electrobunSendToHost({type:"state",n:n,at:Date.now(),signin:signin,loaded:loaded,side:side,title:document.title,text:txt.slice(0,300)});}`,
			`if(n>1200)clearInterval(t);},100);`,
			`})();`,
		);
	}
	return lines.join("");
}

function windowFrame() {
	const o = 40 * (cascade++ % 8);
	return { x: 140 + o, y: 90 + o, width: 1280, height: 820 };
}

function openStudioWindow(opts: {
	key: string;
	title: string;
	origin: string;
	token: string;
	tenant: string | null;
	server?: FileServer;
}): Session {
	const win = new BrowserWindow({
		title: opts.title,
		url: `${opts.origin}/_studio/`,
		frame: windowFrame(),
		preload: handoffScript(opts.origin, opts.token, opts.tenant),
		// No RPC bridge to this process for a page a server serves.
		sandbox: true,
		// Stay on the server's origin; everything else goes to the browser.
		navigationRules: JSON.stringify(["^*", `${opts.origin}/*`]),
	});
	// The constructor's `navigationRules` did not take in 2.0.4 (a navigation to
	// https://example.com reported "allowed": true with it set); the setter does.
	win.webview.setNavigationRules(["^*", `${opts.origin}/*`]);
	const session: Session = { key: opts.key, title: opts.title, win, server: opts.server };
	sessions.set(opts.key, session);

	(win.webview as any).on("new-window-open", (e: any) => {
		const url = e?.data?.detail?.url ?? e?.data?.url ?? e?.data?.detail;
		if (typeof url === "string" && /^https?:/.test(url)) Utils.openExternal(url);
	});
	if (TEST) {
		(win.webview as any).on("host-message", (e: any) => report("page", { key: opts.key, detail: e?.data?.detail ?? e?.data }));
		for (const name of ["will-navigate", "did-navigate", "new-window-open"]) {
			(win.webview as any).on(name, (e: any) => report(name, { key: opts.key, data: JSON.stringify(e?.data ?? e).slice(0, 200) }));
		}
	}
	win.on("close", () => {
		report("window-close-event", { key: opts.key });
		sessions.delete(opts.key);
		if (session.server) {
			session.closing = session.server
				.stop()
				.then((code) => report("server-stopped", { key: opts.key, code }))
				.catch((err) => console.error("could not stop the server", err));
		}
		refreshMenu();
		quitIfNoWindows();
	});
	// A server that dies under an open window (killed, crashed, out of disk).
	opts.server?.exited.then((code) => {
		if (sessions.get(opts.key) === session && !quitting) {
			report("server-died", { key: opts.key, code });
			notice(
				"The database server stopped",
				`${opts.title}: fenec-server exited (${code ?? "signal"}).`,
				opts.server!.log().split("\n").slice(-6).join("\n"),
			);
			session.server = undefined;
			session.win.close();
		}
	});
	return session;
}

// ------------------------------------------------------------------ messages

const escapeHtml = (s: string) => s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

/**
 * A message in a window of its own. Not Utils.showMessageBox: that is a
 * synchronous call into the native layer (`returns: int`) inside an `async`
 * function, so the main process's JavaScript thread -- every server's
 * supervision, a SIGTERM handler, the launcher's RPC -- stands still until
 * someone clicks OK. A test that never clicked held the whole app for good.
 */
function notice(title: string, message: string, detail = "") {
	const html = `<!doctype html><meta charset="utf-8"><meta name="color-scheme" content="light dark">
<style>body{font:14px/1.5 -apple-system,system-ui,sans-serif;margin:22px 24px}h1{font-size:16px;margin:0 0 10px}pre{white-space:pre-wrap;color:gray;font-size:12px;margin:12px 0 0}</style>
<h1>${escapeHtml(title)}</h1><div>${escapeHtml(message)}</div>${detail ? `<pre>${escapeHtml(detail)}</pre>` : ""}`;
	new BrowserWindow({ title, html, sandbox: true, frame: { width: 520, height: 260 } });
}

// ------------------------------------------------------------------ files

type Result = { ok: boolean; message?: string };

async function openFile(raw: string): Promise<Result> {
	let path: string;
	try {
		path = realpathSync(raw);
	} catch {
		return { ok: false, message: `${raw} does not exist.` };
	}
	if (!statSync(path).isFile()) return { ok: false, message: `${path} is not a file.` };

	const open = sessions.get(`file:${path}`);
	if (open) {
		open.win.activate();
		return { ok: false, message: `${basename(path)} is already open in a window of this app; showing it.` };
	}

	let server: FileServer;
	try {
		server = await startFileServer(path);
	} catch (err) {
		if (err instanceof AlreadyOpen) {
			const message = `${basename(path)} is already open in another program (another fenec studio, a fenec-server or an app using the file). Close it there first: two programs over one file would corrupt it.`;
			report("refused", { path, message });
			notice("This database is already open", message, String((err as Error).message));
			return { ok: false, message };
		}
		const message = `Could not open ${basename(path)}: ${(err as Error).message}`;
		report("start-failed", { path, message });
		notice("Could not open the database", basename(path), (err as Error).message);
		return { ok: false, message };
	}
	report("server-started", { path, origin: server.origin, pid: server.pid });
	openStudioWindow({ key: `file:${path}`, title: `${basename(path)} — fenec studio`, origin: server.origin, token: server.token, tenant: null, server });
	store.touch(path);
	refreshMenu();
	pushOverview();
	return { ok: true };
}

async function chooseFile(): Promise<Result> {
	const paths = await Utils.openFileDialog({
		startingFolder: homedir(),
		canChooseFiles: true,
		canChooseDirectory: false,
		allowsMultipleSelection: false,
	});
	const path = paths.find((p) => p);
	return path ? openFile(path) : { ok: true };
}

// ------------------------------------------------------------------ remote servers

function originOf(url: string): string {
	const u = new URL(url.includes("://") ? url : `http://${url}`);
	if (u.protocol !== "http:" && u.protocol !== "https:") throw new Error("A server address starts with http:// or https://");
	return u.origin;
}

async function openConnection(id: string): Promise<Result> {
	const c = store.connections().find((x) => x.id === id);
	if (!c) return { ok: false, message: "That connection is gone." };
	const open = sessions.get(`remote:${c.id}`);
	if (open) {
		open.win.activate();
		return { ok: true };
	}
	const origin = originOf(c.url);
	const token = (await secrets.get(`connection:${c.id}`)) ?? "";
	// Say what is wrong in words before a blank window does: the studio is
	// served by the server itself and only when it was started with --studio.
	try {
		const res = await fetch(`${origin}/_studio/`, { signal: AbortSignal.timeout(6000), redirect: "manual" });
		// Served before any token is asked, so an answer that is not the page
		// is a server with the studio off (404) or one older than the studio.
		if (!(res.ok && (res.headers.get("content-type") ?? "").includes("html"))) {
			return {
				ok: false,
				message: `${origin} answers (HTTP ${res.status}) but does not serve a studio: start that fenec-server (or fenec-shard) with --studio, from a build that has it.`,
			};
		}
	} catch (err) {
		const e = err as Error & { code?: string };
		return { ok: false, message: `Could not reach ${origin} (${e.code ?? e.name}): is the server running, and is the address right?` };
	}
	openStudioWindow({ key: `remote:${c.id}`, title: `${c.name} — fenec studio`, origin, token, tenant: c.tenant });
	refreshMenu();
	return { ok: true };
}

async function saveConnection({ name, url, token, tenant }: { name: string; url: string; token: string; tenant: string }): Promise<string> {
	const origin = originOf(url);
	const id = crypto.randomUUID();
	store.saveConnection({ id, name: name.trim() || new URL(origin).host, url: origin, tenant: tenant.trim() || null });
	// The token goes to the keychain and nowhere else on disk.
	if (token) await secrets.set(`connection:${id}`, token);
	return id;
}

async function removeConnection(id: string) {
	store.removeConnection(id);
	await secrets.delete(`connection:${id}`);
}

// ------------------------------------------------------------------ launcher

async function overview(): Promise<Overview> {
	const connections = await Promise.all(
		store.connections().map(async (c) => ({ ...c, hasToken: ((await secrets.get(`connection:${c.id}`)) ?? "") !== "" })),
	);
	return {
		recents: store.recents().map((r) => ({ ...r, name: basename(r.path), exists: existsSync(r.path) })),
		connections,
		open: [...sessions.keys()],
		keychain: secrets.kind,
	};
}

const launcherRPC = BrowserView.defineRPC<LauncherRPC>({
	maxRequestTime: 60_000,
	handlers: {
		requests: {
			overview: () => overview(),
			chooseFile: () => chooseFile(),
			openPath: ({ path }) => openFile(path),
			forgetRecent: ({ path }) => {
				store.forgetRecent(path);
				refreshMenu();
				return overview();
			},
			saveConnection: async (c) => {
				try {
					await saveConnection(c);
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

function showLauncher() {
	if (launcher) {
		launcher.activate();
		return;
	}
	launcher = new BrowserWindow({
		title: "fenec studio",
		url: "views://launcher/index.html",
		rpc: launcherRPC,
		frame: { x: 120, y: 120, width: 760, height: 640 },
		// Test hook only: what the launcher shows, through the event bridge.
		preload: TEST
			? `setInterval(function(){var t=document.body?document.body.innerText:"";window.__electrobunSendToHost&&window.__electrobunSendToHost({type:"launcher",text:t.replace(/\\s+/g," ").slice(0,600)})},700);`
			: undefined,
	});
	if (TEST) {
		let last = "";
		(launcher.webview as any).on("host-message", (e: any) => {
			const d = e?.data?.detail ?? e?.data;
			if (d?.text !== last) report("launcher", { text: (last = d?.text) });
		});
	}
	launcher.on("close", () => {
		launcher = null;
		quitIfNoWindows();
	});
}

function pushOverview() {
	// The launcher asks again when it is focused; nothing to push in the spike.
}

/** Windows and Linux apps end with their last window; a macOS one stays in the Dock. */
function quitIfNoWindows() {
	if (process.platform === "darwin" || sessions.size > 0 || launcher) return;
	Utils.quit();
}

// ------------------------------------------------------------------ menu

function refreshMenu() {
	const recents = store.recents().slice(0, 10);
	ApplicationMenu.setApplicationMenu([
		{
			label: "fenec studio",
			submenu: [{ label: "About fenec studio", role: "about" } as any, { type: "separator" }, { role: "hide" } as any, { role: "quit" }],
		},
		{
			label: "File",
			submenu: [
				{ label: "Open…", action: "open", accelerator: "o" },
				{
					label: "Open Recent",
					submenu: recents.length
						? recents.map((r) => ({ label: basename(r.path), action: "open-recent", data: { path: r.path } }))
						: [{ label: "No recent files", enabled: false }],
				},
				{ label: "Connect to a Server…", action: "launcher", accelerator: "shift+o" } as any,
				{ type: "separator" },
				{ role: "close" },
			],
		},
		{
			label: "Edit",
			submenu: [
				{ role: "undo" },
				{ role: "redo" },
				{ type: "separator" },
				{ role: "cut" },
				{ role: "copy" },
				{ role: "paste" },
				{ role: "selectAll" },
			],
		},
	] as any);
}

ApplicationMenu.on("application-menu-clicked", (event: any) => {
	const d = event?.data ?? event;
	const action = d?.action;
	const data = d?.data;
	report("menu", { action, data });
	if (action === "open") void chooseFile();
	else if (action === "open-recent" && data?.path) void openFile(data.path);
	else if (action === "launcher") showLauncher();
});

// ------------------------------------------------------------------ app lifecycle

// Finder: a .fenec file double-clicked (macOS; Info.plist from app.fileAssociations).
Electrobun.events.on("open-url", (event: any) => {
	const url = String(event?.data?.url ?? "");
	report("open-url", { url });
	if (url.startsWith("file:")) void openFile(decodeURIComponent(new URL(url).pathname));
});

Electrobun.events.on("reopen", () => {
	if (sessions.size === 0) showLauncher();
});

// Stop every server before the process goes: the quit is held, the servers
// are asked to checkpoint, then the quit is asked for again.
Electrobun.events.on("before-quit", (event: any) => {
	if (quitting) return;
	const running = [...sessions.values()].filter((s) => s.server);
	if (running.length === 0) return;
	quitting = true;
	event.response = { allow: false };
	Promise.all(running.map((s) => s.server!.stop())).finally(() => Utils.quit());
});

for (const sig of ["SIGTERM", "SIGINT", "SIGHUP"] as const) {
	process.on(sig, () => {
		quitting = true;
		Promise.all([...sessions.values()].map((s) => s.server?.stop())).finally(() => process.exit(0));
	});
}

// Test control: a file of JSON commands, one a line, read as it grows. Only
// with FENEC_DESKTOP_CONTROL set; the shipped app sets nothing and reads no file.
const CONTROL = process.env["FENEC_DESKTOP_CONTROL"];
if (CONTROL) {
	let offset = 0;
	const sessionFor = (key: string) => sessions.get(key);
	const run = async (c: any) => {
		report("cmd", { cmd: c });
		switch (c.cmd) {
			case "open": report("open-result", await openFile(c.path)); break;
			case "close": sessionFor(c.key)?.win.requestClose(); break; // the path the red button takes
			case "close-direct": sessionFor(c.key)?.win.close(); break;
			// executeJavascript is not the page's own eval(): the studio's CSP
			// (script-src 'self') does not apply to it. The code reports its
			// result itself, through the event-only bridge.
			case "eval": sessionFor(c.key)?.win.webview.executeJavascript(c.js); break;
			case "sessions": report("sessions", { keys: [...sessions.keys()], pids: [...sessions.values()].map((s) => s.server?.pid) }); break;
			case "addconn": report("conn-added", { id: await saveConnection(c) }); break;
			case "openconn": report("openconn-result", await openConnection(c.id)); break;
			case "rmconn": await removeConnection(c.id); break;
			case "secret": report("secret", { name: c.name, value: await secrets.get(c.name) }); break;
			case "quit": Utils.quit(); break;
		}
	};
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

const strays = await reapStrays();
if (strays.length) report("reaped-strays", { strays });
refreshMenu();
report("ready");

const autoOpen = process.env["FENEC_DESKTOP_OPEN"];
if (autoOpen) {
	for (const p of autoOpen.split(":")) await openFile(p);
}
// A launch from Finder with a file arrives as `open-url` a moment after the
// process starts: wait for it before showing the launcher over the file's window.
if (!autoOpen) await Bun.sleep(400);
if (!autoOpen && sessions.size === 0) showLauncher();
else if (process.env["FENEC_DESKTOP_LAUNCHER"]) showLauncher();
