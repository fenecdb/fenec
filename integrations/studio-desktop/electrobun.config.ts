import type { ElectrobunConfig } from "electrobun";

export default {
	app: {
		name: "fenec studio",
		identifier: "dev.fenecdb.studio",
		version: "0.0.1",
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
		bun: {
			entrypoint: "src/bun/index.ts",
		},
		views: {
			// The launcher: recent files and saved connections. Bundled, trusted,
			// the only page that gets the RPC bridge. Studio windows get none.
			launcher: {
				entrypoint: "src/launcher/index.ts",
			},
		},
		copy: {
			"src/launcher/index.html": "views/launcher/index.html",
			"src/launcher/index.css": "views/launcher/index.css",
			// The server the app starts for a file: put here by scripts/get-server.sh
			// (a build of this checkout, or a release asset once one carries --studio).
			"bin/fenec-server": "bin/fenec-server",
		},
		mac: {
			bundleCEF: false,
			// Signing and notarization need a Developer ID certificate; see the
			// PR description. Off here so `build` works on any machine.
			codesign: false,
			notarize: false,
			createDmg: true,
		},
		linux: {
			bundleCEF: false,
		},
		win: {
			bundleCEF: false,
		},
	},
} satisfies ElectrobunConfig;
