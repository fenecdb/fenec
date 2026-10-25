import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

// The build tool is named in four places that must move together: the
// dependency in package.json, the version hutch.config.ts builds with, the
// Hutch release it pairs with, and the hashes in hutch-pin.json that
// scripts/electrobun.mjs holds the downloaded tool to.

const read = (name: string) => readFileSync(new URL(`../${name}`, import.meta.url), "utf8");
const pin = JSON.parse(read("hutch-pin.json"));
const pkg = JSON.parse(read("package.json"));

test("the pin is for the Electrobun the package depends on, the one hutch.config.ts builds with, and the Hutch it pairs with", () => {
	assert.equal(pkg.devDependencies.electrobun, pin.electrobun, "an exact version, not a range");
	assert.match(read("hutch.config.ts"), new RegExp(`version: "${pin.electrobun.replaceAll(".", "\\.")}"`));
	assert.match(read("hutch.config.ts"), new RegExp(`// @hutch cli=${pin.hutch.replaceAll(".", "\\.")} `));
});

test("every hash in the pin is a SHA-256", () => {
	const mac = pin["macos-arm64"];
	for (const h of [mac.archive, ...Object.values<string>(mac.files)]) assert.match(h, /^[0-9a-f]{64}$/);
	assert.deepEqual(Object.keys(mac.files).sort(), ["bin/hutch", "bin/hutch-engine"]);
});

test("what Hutch fetches, and what ships inside the app, is pinned: the Electrobun runtime, the Bun, and Cottontail", () => {
	const runtime = pin["macos-arm64"].runtime as Record<string, { files: number; tree: string }>;
	assert.deepEqual(
		Object.keys(runtime).map((k) => k.split("/").slice(0, 3).join("/")),
		[`releases/electrobun/${pin.electrobun}`, "toolchains/bun/1.4.0", "releases/cottontail/0.7.2"],
	);
	for (const [dir, want] of Object.entries(runtime)) {
		assert.match(want.tree, /^[0-9a-f]{64}$/, dir);
		assert.ok(Number.isInteger(want.files) && want.files > 0, dir);
	}
	assert.match(read("hutch.config.ts"), /cottontail=0\.7\.2/);
});

test("the dependencies are exact versions", () => {
	for (const [name, version] of Object.entries<string>({ ...pkg.devDependencies })) assert.match(version, /^\d+\.\d+\.\d+$/, name);
});
