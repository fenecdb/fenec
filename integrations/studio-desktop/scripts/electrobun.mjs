#!/usr/bin/env node
// Runs Electrobun's build tool (Hutch) from inside this directory, installing
// nothing outside it.
//
//   node scripts/electrobun.mjs dev | build --env=stable | ...
//
// `electrobun` (the pinned devDependency) is only a bootstrap: on first use it
// downloads the Hutch release paired with its own version from Electrobun's
// GitHub release, checks the archive's SHA-256 against the release's index
// and unpacks it under $HUTCH_HOME. Left alone it would use ~/.hutch, and,
// were that download to fail, fall back to Hutch's install.sh, which installs
// Hutch globally and appends a PATH line to the user's shell rc file. So this
// does four things its bin does not:
//   - HUTCH_HOME is .hutch-home/ here (gitignored; the Hutch binary, its Bun
//     and the Electrobun runtime all go in it) unless the caller set one;
//   - it hands the resolver an installer that refuses, so no path ends in
//     install.sh and no shell rc is ever touched;
//   - it holds Hutch to hutch-pin.json: the archive's SHA-256 as downloaded,
//     and the SHA-256 of the two binaries it holds, every run. The release's
//     index is fetched from the same place as the archive and proves little
//     against that place; the pin is in this repository, and a restored cache
//     (CI keeps .hutch-home between runs) cannot stand in for it;
//   - it runs the resolver's own functions, not `bin/electrobun.cjs`, to be able to do that.
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

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
			"take the new archive's and binaries' SHA-256 from the release (scripts/electrobun.mjs says which) and write them in hutch-pin.json",
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
});

// And what is on disk, every time, cached or just unpacked.
const root = dirname(dirname(binary));
for (const [file, want] of Object.entries(pinned.files)) {
	let got;
	try {
		got = sha256(readFileSync(join(root, file)));
	} catch (e) {
		die(`${join(root, file)} cannot be read: ${e.message}`);
	}
	if (got !== want) die(`${join(root, file)} has SHA-256 ${got}, hutch-pin.json says ${want}: delete ${root} to fetch it again, and do not run it`);
}

process.exitCode = resolver.runHutch({ binary, args: ["electrobun", ...process.argv.slice(2)], environment });
