import type { ElectrobunConfig } from "electrobun";
import { readFileSync } from "node:fs";

const { version } = JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf8")) as { version: string };

// A variable set empty is unset (a CI secret that does not exist reaches a
// step as an empty string).
const set = (name: string) => (process.env[name] ?? "").trim() !== "";

// The test variant is the same app built with the hooks the scripted checks
// drive (src/bun/testhooks.ts), under an identifier of its own, so a run
// reads and writes none of a real install's recent files, saved servers or
// Keychain items. The release build holds none of that code: `__TESTABLE__`
// is a build-time constant, false there, and the bundler drops the branch.
// scripts/checks/release.mjs holds the shipped bundle to it.
const test = process.env["FENEC_DESKTOP_TEST_BUILD"] === "1";

// Signing and notarization happen only where their secrets exist, so a
// build on a laptop or in a fork's pull request is an unsigned one and
// still works. Notarization takes an App Store Connect API key or an Apple
// ID with an app-specific password; the certificate itself is imported into
// a keychain by the caller (.github/workflows/desktop.yml does).
const sign = !test && set("ELECTROBUN_DEVELOPER_ID");
const notarize =
	sign &&
	((set("ELECTROBUN_APPLEAPIKEY") && set("ELECTROBUN_APPLEAPIISSUER") && set("ELECTROBUN_APPLEAPIKEYPATH")) ||
		(set("ELECTROBUN_APPLEID") && set("ELECTROBUN_APPLEIDPASS") && set("ELECTROBUN_TEAMID")));

const identifier = test ? "dev.fenecdb.studio.test" : "dev.fenecdb.studio";

export default {
	app: {
		name: test ? "fenec studio test" : "fenec studio",
		identifier,
		version,
		description: "fenec studio as a desktop app: open a database file, or connect to a server.",
		// A double-click on a .fenec file in Finder arrives as an `open-url`
		// event with a file: URL (macOS only; Windows and Linux file
		// associations are not implemented by Electrobun 2.0.4).
		fileAssociations: [{ name: "fenecdb database", ext: ["fenec"], role: "Editor" }],
	},
	runtime: {
		// The default quits the process as the last window closes, right after the
		// close handlers return: too soon for a server to finish its SIGTERM
		// (sync + checkpoint) while we still hold its lock. The app stays up
		// with no window, as a macOS document app does, and stops the servers
		// itself on quit (src/bun/index.ts).
		exitOnLastWindowClosed: false,
	},
	build: {
		mainProcess: "bun",
		buildFolder: test ? "build-test" : "build",
		artifactFolder: test ? "artifacts-test" : "artifacts",
		bun: {
			entrypoint: "src/bun/index.ts",
			define: {
				__TESTABLE__: JSON.stringify(test),
				// The Keychain service the saved servers' tokens go under.
				__KEYCHAIN_SERVICE__: JSON.stringify(identifier),
			},
		},
		views: {
			// Pages of the app's own, bundled. The launcher and the new-database
			// window are trusted and get the RPC bridge to the main process; the
			// notice window is sandboxed and can only say it was clicked. A studio
			// window (a page a server serves) gets no bridge at all.
			launcher: { entrypoint: "src/launcher/index.ts" },
			newdb: { entrypoint: "src/newdb/index.ts" },
			notice: { entrypoint: "src/notice/index.ts" },
		},
		copy: {
			"src/launcher/index.html": "views/launcher/index.html",
			"src/newdb/index.html": "views/newdb/index.html",
			"src/notice/index.html": "views/notice/index.html",
			"src/ui/app.css": "views/ui/app.css",
			// The server the app starts for a file: put here by scripts/get-server.sh.
			"bin/fenec-server": "bin/fenec-server",
		},
		mac: {
			bundleCEF: false,
			// Hutch signs the nested Mach-O files (the bundled fenec-server among
			// them), the app and the DMG, submits it, staples and validates. The
			// server needs no entitlement of its own.
			codesign: sign,
			notarize,
			createDmg: !test,
			icons: "icon.iconset",
		},
		linux: {
			bundleCEF: false,
		},
		win: {
			bundleCEF: false,
		},
	},
	// No update feed yet: Electrobun makes bsdiff patches only against a
	// release it can download, and nothing is published (README, "Updates").
	release: { generatePatch: false },
} satisfies ElectrobunConfig;
