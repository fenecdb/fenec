#!/usr/bin/env node
// Runs Electrobun's build tool (Hutch) from inside this directory, installing
// nothing outside it.
//
//   node scripts/electrobun.mjs dev | build --env=stable | ...
//
// `electrobun` (the pinned devDependency) is only a bootstrap: on first use it
// downloads the Hutch release paired with its own version from Electrobun's
// GitHub release, checks the archive's SHA-256 against the release's index and
// unpacks it under $HUTCH_HOME. Left alone it would use ~/.hutch, and, were
// that download to fail, fall back to Hutch's install.sh, which installs
// Hutch globally and appends a PATH line to the user's shell rc file. So this
// does three things its bin does not:
//   - HUTCH_HOME is .hutch-home/ here (gitignored; the Hutch binary, its Bun
//     and the Electrobun runtime all go in it) unless the caller set one;
//   - it hands the resolver an installer that refuses, so no path ends in
//     install.sh and no shell rc is ever touched;
//   - it runs the resolver's own functions, not `bin/electrobun.cjs`, to be able to do that.
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = join(dirname(fileURLToPath(import.meta.url)), "..");
const require = createRequire(import.meta.url);
// By path: the package's "exports" map every subpath to a stub that only
// says the package moved.
const resolver = require(join(here, "node_modules", "electrobun", "bin", "resolve-hutch.cjs"));

const environment = { ...process.env, HUTCH_HOME: process.env.HUTCH_HOME || join(here, ".hutch-home") };

const binary = await resolver.resolveHutchBinary({
	environment,
	installHutch: async () => {
		throw new Error(
			"the paired Hutch could not be downloaded, and this project never runs Hutch's global installer " +
				"(it edits shell rc files). Check the network, or set ELECTROBUN_RELEASES_BASE_URL to a mirror of Electrobun's releases.",
		);
	},
});
process.exitCode = resolver.runHutch({ binary, args: ["electrobun", ...process.argv.slice(2)], environment });
