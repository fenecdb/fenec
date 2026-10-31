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
import { sweepSecrets } from "./clipboard.ts";
import { claimInstance, instancePath } from "./instance.ts";
import { startMenu } from "./menu.ts";
import { closeLauncher, launcherOpen, showLauncher } from "./pages.ts";

// One instance at a time (instance.ts): a second asks the first to show its
// main window and quits before it opens anything or reads its lists.
const alone = await claimInstance(instancePath(__KEYCHAIN_SERVICE__), () => {
	if (__TESTABLE__) void import("./testhooks.ts").then((h) => h.report("instance-asked"));
	void showLauncher();
});
if (!alone) {
	if (__TESTABLE__) (await import("./testhooks.ts")).report("second-instance");
	Utils.quit();
	// Nothing after this runs in a second instance.
	await new Promise(() => {});
}

const started = Date.now();
/** The launcher was shown because nothing else was opening, not because someone asked for it. */
let autoLauncher = false;
/** A file was handed over by the system (Finder), and when. */
let handedOver = false;
const AUTO_LAUNCHER_GRACE_MS = 5000;

// Finder: a .fenec file double-clicked (macOS; Info.plist from app.fileAssociations).
Electrobun.events.on("open-url", (event: any) => {
	const url = String(event?.data?.url ?? "");
	if (__TESTABLE__) void import("./testhooks.ts").then((h) => h.report("open-url", { url }));
	if (url.startsWith("file:")) {
		void (async () => {
			// The test variant can hold the event back until the launcher is up, as a
			// slow launch delivers it after: the order a timer could only hope for.
			if (__TESTABLE__ && process.env["FENEC_DESKTOP_HOLD_OPEN_URL"] === "launcher") {
				for (let i = 0; i < 200 && !launcherOpen(); i++) await Bun.sleep(50);
			}
			// A file handed over after the launcher was shown on its own (the launch
			// was slow, and the 400 ms below was not enough) is what the launch was
			// for: the launcher, which nobody has had time to use, is closed.
			handedOver = true;
			if (autoLauncher && Date.now() - started < AUTO_LAUNCHER_GRACE_MS) closeLauncher();
			void openFile(decodeURIComponent(new URL(url).pathname));
		})();
	}
});

// The Dock icon clicked with no window open.
Electrobun.events.on("reopen", () => {
	if (!anythingOpen()) void showLauncher();
});

// Stop every server before the process goes: the quit is held, the servers
// are asked to checkpoint, then the quit is asked for again. A token copied
// less than a minute ago goes from the clipboard first (clipboard.ts).
Electrobun.events.on("before-quit", (event: any) => {
	sweepSecrets();
	if (isQuitting() || !serversBusy()) return;
	event.response = { allow: false };
	void stopAll().finally(() => Utils.quit());
});

for (const sig of ["SIGTERM", "SIGINT", "SIGHUP"] as const) {
	process.on(sig, () => {
		sweepSecrets();
		void stopAll().finally(() => process.exit(0));
	});
}

// A server this app started does not outlive it: its stdin is a pipe this
// process holds, and the server stops when it ends (server.ts), so there is
// nothing from an earlier run to stop before a file is opened.
startMenu();

let opened = false;
if (__TESTABLE__) {
	const hooks = await import("./testhooks.ts");
	opened = await hooks.boot();
}
// A launch from Finder with a file arrives as `open-url` a moment after the
// process starts: wait for it before showing the launcher over the file's window.
if (!opened) await Bun.sleep(400);
if (!opened && !anythingOpen()) {
	autoLauncher = true;
	await showLauncher();
	// The file may have come while the launcher was being made.
	if (handedOver) closeLauncher();
}
