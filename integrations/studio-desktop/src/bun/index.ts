// fenec studio, desktop. The main process (Bun).
//
// A window per open thing:
//   - a file: a fenec-server this process started for it, and a window on
//     that server's own /_studio/ (so the studio is the one the server was built with);
//   - a remote server: a window on that server's /_studio/, same origin, so no CORS;
// plus a launcher (recent files, saved servers) and a new-database window.
//
// The pieces: app.ts what the app does, server.ts the one place a server is
// started, session.ts the one place a token reaches a page, windows.ts what a
// studio window is held to, pages.ts the app's own pages, messages.ts every
// failure in words. README.md has the picture.

import Electrobun, { Utils } from "electrobun/main";
import { anythingOpen, isQuitting, openFile, serversBusy, stopAll } from "./app.ts";
import { startMenu } from "./menu.ts";
import { showLauncher } from "./pages.ts";
import { strays } from "./registry.ts";

// Finder: a .fenec file double-clicked (macOS; Info.plist from app.fileAssociations).
Electrobun.events.on("open-url", (event: any) => {
	const url = String(event?.data?.url ?? "");
	if (__TESTABLE__) void import("./testhooks.ts").then((h) => h.report("open-url", { url }));
	if (url.startsWith("file:")) void openFile(decodeURIComponent(new URL(url).pathname));
});

// The Dock icon clicked with no window open.
Electrobun.events.on("reopen", () => {
	if (!anythingOpen()) void showLauncher();
});

// Stop every server before the process goes: the quit is held, the servers
// are asked to checkpoint, then the quit is asked for again.
Electrobun.events.on("before-quit", (event: any) => {
	if (isQuitting() || !serversBusy()) return;
	event.response = { allow: false };
	void stopAll().finally(() => Utils.quit());
});

for (const sig of ["SIGTERM", "SIGINT", "SIGHUP"] as const) {
	process.on(sig, () => {
		void stopAll().finally(() => process.exit(0));
	});
}

// Servers an earlier run left behind (strays.ts) go before anything opens a
// file, and this waits until each is gone: the file's lock is theirs till then.
const stray = await strays.reap();
if (stray.left.length) console.error("a server of an earlier run would not stop:", JSON.stringify(stray.left));
startMenu();

let opened = false;
if (__TESTABLE__) {
	const hooks = await import("./testhooks.ts");
	if (stray.reaped.length || stray.left.length) hooks.report("reaped-strays", { strays: stray.reaped, left: stray.left });
	opened = await hooks.boot();
}
// A launch from Finder with a file arrives as `open-url` a moment after the
// process starts: wait for it before showing the launcher over the file's window.
if (!opened) await Bun.sleep(400);
if (!opened && !anythingOpen()) await showLauncher();
