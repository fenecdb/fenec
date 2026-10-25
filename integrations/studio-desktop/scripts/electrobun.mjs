#!/usr/bin/env node
// Runs Electrobun's build tool (Hutch) from inside this directory, installing
// nothing outside it.
//
//   node scripts/electrobun.mjs dev | build --env=stable | ...
//   node scripts/electrobun.mjs --print-runtime-pin     # what hutch-pin.json should hold
//
// `electrobun` (the pinned devDependency) is only a bootstrap: on first use it
// downloads the Hutch release paired with its own version from Electrobun's
// GitHub release, checks the archive's SHA-256 against the release's index
// and unpacks it under $HUTCH_HOME. Left alone it would use ~/.hutch, and,
// were that download to fail, fall back to Hutch's install.sh, which installs
// Hutch globally and appends a PATH line to the user's shell rc file. So this
// does five things its bin does not:
//   - HUTCH_HOME is .hutch-home/ here (gitignored; the Hutch binary, its Bun
//     and the Electrobun runtime all go in it) unless the caller set one;
//   - it hands the resolver an installer that refuses, so no path ends in
//     install.sh and no shell rc is ever touched;
//   - it holds Hutch to hutch-pin.json: the archive's SHA-256 as downloaded,
//     and the SHA-256 of the two binaries it holds, every run. The release's
//     index is fetched from the same place as the archive and proves little
//     against that place; the pin is in this repository, and a restored cache
//     (CI keeps .hutch-home between runs) cannot stand in for it;
//   - it holds what Hutch then fetches to the same pin, before the build uses
//     it: the Electrobun runtime (the native library, the launcher, the main
//     process's API sources, the preloads: all of it ships inside the app), the
//     Bun it bundles and runs, and Cottontail. Hutch checks them only against
//     a lock it writes beside the project (.hutch/dependencies.lock, not in git,
//     and with no hash for its Bun), so a tampered file there would pass. They
//     are fetched first (`sync`, which runs nothing of the app's), checked,
//     and only then is the command run;
//   - it never lets the resolver run a Hutch it did not unpack from the
//     pinned archive: the resolver's fallback to "a compatible Hutch already
//     at $HUTCH_HOME/bin" is switched off, and so is ELECTROBUN_HUTCH_BINARY.
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { treeDigest } from "./tree-digest.mjs";

const here = join(dirname(fileURLToPath(import.meta.url)), "..");
const require = createRequire(import.meta.url);
// By path: the package's "exports" map every subpath to a stub that only
// says the package moved.
const resolver = require(join(here, "node_modules", "electrobun", "bin", "resolve-hutch.cjs"));
const pin = JSON.parse(readFileSync(join(here, "hutch-pin.json"), "utf8"));
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const die = (message) => {
	console.error(`electrobun: ${message}`);
	process.exit(1);
};

// The pin names the Electrobun it is for; moving the dependency means moving it.
const installed = JSON.parse(readFileSync(join(here, "node_modules", "electrobun", "package.json"), "utf8")).version;
if (installed !== pin.electrobun || resolver.PAIRED_HUTCH_VERSION !== pin.hutch) {
	die(
		`hutch-pin.json pins Electrobun ${pin.electrobun} with Hutch ${pin.hutch}, and this is Electrobun ${installed} with Hutch ${resolver.PAIRED_HUTCH_VERSION}: ` +
			"take the new archive's, binaries' and runtime's SHA-256 (`node scripts/electrobun.mjs --print-runtime-pin` prints the runtime's) and write them in hutch-pin.json",
	);
}
const platformKey = resolver.hutchPlatformKey(process.platform, process.arch);
const pinned = platformKey ? pin[platformKey] : undefined;
if (!pinned) die(`no Hutch is pinned for ${process.platform} ${process.arch} (hutch-pin.json has: ${Object.keys(pin).filter((k) => k.includes("-")).join(", ")}); this app builds on macOS with Apple silicon`);

// A binary named in the environment is taken by the resolver unchecked.
const environment = { ...process.env, HUTCH_HOME: process.env.HUTCH_HOME || join(here, ".hutch-home") };
if (environment.ELECTROBUN_HUTCH_BINARY) {
	console.error("electrobun: ignoring ELECTROBUN_HUTCH_BINARY: the Hutch of hutch-pin.json is used");
	delete environment.ELECTROBUN_HUTCH_BINARY;
}

const binary = await resolver.resolveHutchBinary({
	environment,
	// The resolver, when it cannot get the paired Hutch, uses one that is
	// "compatible" already at $HUTCH_HOME/bin (a cache's, unpinned) if the file is
	// there. It is not there for it: this is the only thing it asks the file system.
	existsSync: () => false,
	installHutch: async () => {
		throw new Error(
			"the paired Hutch could not be downloaded, and this project never runs Hutch's global installer " +
				"(it edits shell rc files). Check the network, or set ELECTROBUN_RELEASES_BASE_URL to a mirror of Electrobun's releases.",
		);
	},
	// The archive, as it came: compared with the pin before it is unpacked.
	download: async (url, options) => {
		const bytes = await resolver.download(url, options);
		if (/Hutch archive$/.test(options?.label ?? "") && sha256(bytes) !== pinned.archive) {
			die(`the Hutch archive downloaded from ${url} has SHA-256 ${sha256(bytes)}, hutch-pin.json says ${pinned.archive}: not using it`);
		}
		return bytes;
	},
})
	.catch((e) => die(e.message));

// And what is on disk, every time, cached or just unpacked.
const root = dirname(dirname(binary));
if (root !== resolver.downloadedHutchRoot(environment, process.platform, process.env.HOME ?? "", platformKey)) {
	die(`the resolver chose ${binary}, which is not the Hutch unpacked from the pinned archive`);
}
for (const [file, want] of Object.entries(pinned.files)) {
	let got;
	try {
		got = sha256(readFileSync(join(root, file)));
	} catch (e) {
		die(`${join(root, file)} cannot be read: ${e.message}`);
	}
	if (got !== want) die(`${join(root, file)} has SHA-256 ${got}, hutch-pin.json says ${want}: delete ${root} to fetch it again, and do not run it`);
}

/** What Hutch fetched into HUTCH_HOME, held to the pin. `null` for none that differs, else what differs. */
function runtimeProblems() {
	const home = environment.HUTCH_HOME;
	const problems = [];
	for (const [dir, want] of Object.entries(pinned.runtime ?? {})) {
		if (!existsSync(join(home, dir))) {
			problems.push(`${dir} is not there`);
			continue;
		}
		const got = treeDigest(join(home, dir));
		if (got.tree !== want.tree) problems.push(`${dir}: ${got.files} files, tree ${got.tree}; hutch-pin.json says ${want.files} files, tree ${want.tree}`);
	}
	return problems;
}

const args = process.argv.slice(2);

if (args[0] === "--print-runtime-pin") {
	const out = {};
	for (const dir of Object.keys(pinned.runtime ?? {})) out[dir] = treeDigest(join(environment.HUTCH_HOME, dir));
	console.log(JSON.stringify(out, null, "\t"));
	process.exit(0);
}

const run = (list) => resolver.runHutch({ binary, args: ["electrobun", ...list], environment });
const needsRuntime = ["build", "dev", "run", "prepare", "sync", "config"].includes(args[0] ?? "");

if (needsRuntime) {
	// Fetch the runtime without running anything of the app's (`sync` makes the
	// type declarations beside the project and downloads what a build uses), then
	// look at it, before the build or the app starts.
	if (args[0] !== "sync") {
		const status = run(["sync", ...args.filter((a) => a.startsWith("--env="))]);
		if (status !== 0) process.exit(status);
	}
	const problems = args[0] === "sync" ? [] : runtimeProblems();
	if (problems.length) die(`what Hutch fetched is not what hutch-pin.json pins:\n  ${problems.join("\n  ")}\ndelete .hutch-home/ to fetch it again, and do not build with it`);
}
const status = run(args);
if (needsRuntime && args[0] === "sync") {
	const problems = runtimeProblems();
	if (problems.length) die(`what Hutch fetched is not what hutch-pin.json pins:\n  ${problems.join("\n  ")}\ndelete .hutch-home/ to fetch it again, and do not build with it`);
}
process.exitCode = status;
