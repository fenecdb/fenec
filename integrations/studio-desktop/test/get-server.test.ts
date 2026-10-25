import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";

// scripts/get-server.sh against a release laid out as GitHub lays one out
// (`v<V>/fenec-v<V>-<triple>.tar.gz` and `SHA256SUMS`) in a directory, which
// it reads through file://: no network, and a server that is a shell script.

const script = new URL("../scripts/get-server.sh", import.meta.url).pathname;
const work = mkdtempSync(join(tmpdir(), "fenec-get-server-"));
after(() => rmSync(work, { recursive: true, force: true }));

const TRIPLE = "aarch64-apple-darwin";
const sha = (file: string) => execFileSync("shasum", ["-a", "256", file]).toString().split(" ")[0]!;

/** A release `version` whose server prints `help` for --help. */
function release(version: string, help: string) {
	const dir = join(work, "releases", `v${version}`);
	const stage = join(work, `stage-${version}`, `fenec-v${version}-${TRIPLE}`);
	mkdirSync(dir, { recursive: true });
	mkdirSync(stage, { recursive: true });
	writeFileSync(join(stage, "fenec-server"), `#!/bin/sh\necho '${help}'\n`);
	chmodSync(join(stage, "fenec-server"), 0o755);
	const asset = `fenec-v${version}-${TRIPLE}.tar.gz`;
	execFileSync("tar", ["-czf", join(dir, asset), "-C", join(work, `stage-${version}`), `fenec-v${version}-${TRIPLE}`]);
	// Another asset first, as a real SHA256SUMS lists many.
	writeFileSync(join(dir, "SHA256SUMS"), `${"0".repeat(64)}  fenec-web-v${version}.tar.gz\n${sha(join(dir, asset))}  ${asset}\n`);
	return { dir, asset };
}

function get(version: string, out: string) {
	return spawnSync("sh", [script, version], {
		encoding: "utf8",
		env: { ...process.env, FENEC_RELEASES: `file://${join(work, "releases")}`, FENEC_SERVER_OUT: out, TARGET_TRIPLE: TRIPLE },
	});
}

test("a release's asset is installed once its SHA-256 matches the release's own", () => {
	release("9.0.1", "usage: fenec-server --file <path> --studio");
	const out = join(work, "ok", "fenec-server");
	const r = get("9.0.1", out);
	assert.equal(r.status, 0, r.stderr);
	assert.ok(existsSync(out));
	assert.match(r.stdout, /matches the release's SHA256SUMS/);
	assert.match(execFileSync(out).toString(), /--studio/);
	assert.ok(!existsSync(`${out}.new`), "no staging file left behind");
});

test("a v in front of the version is fine", () => {
	const out = join(work, "v", "fenec-server");
	assert.equal(get("v9.0.1", out).status, 0);
	assert.ok(existsSync(out));
});

test("an asset that is not what the release says is refused, and a server already there is kept", () => {
	const { dir, asset } = release("9.0.2", "usage: --studio");
	// The download changed after the sums were made.
	writeFileSync(join(dir, asset), readFileSync(join(dir, asset)).toString("latin1") + "x", "latin1");
	const out = join(work, "tampered", "fenec-server");
	mkdirSync(join(work, "tampered"));
	writeFileSync(out, "the server already here");
	const r = get("9.0.2", out);
	assert.notEqual(r.status, 0);
	assert.match(r.stderr, /SHA-256/);
	assert.match(r.stderr, /not using it/);
	assert.equal(readFileSync(out, "utf8"), "the server already here");
});

test("a release whose sums do not list the asset is refused", () => {
	const { dir } = release("9.0.3", "usage: --studio");
	writeFileSync(join(dir, "SHA256SUMS"), `${"0".repeat(64)}  something-else.tar.gz\n`);
	const r = get("9.0.3", join(work, "unlisted", "fenec-server"));
	assert.notEqual(r.status, 0);
	assert.match(r.stderr, /does not list fenec-v9\.0\.3/);
});

test("a server without --studio is refused with the way out, and installs nothing", () => {
	release("9.0.4", "usage: fenec-server --file <path> --http <addr>");
	const out = join(work, "old", "fenec-server");
	const r = get("9.0.4", out);
	assert.notEqual(r.status, 0);
	assert.match(r.stderr, /no --studio/);
	assert.match(r.stderr, /build the one in this checkout/);
	assert.ok(!existsSync(out));
});

test("a release that does not exist, and a version that is not one", () => {
	const missing = get("9.9.9", join(work, "none", "fenec-server"));
	assert.notEqual(missing.status, 0);
	assert.match(missing.stderr, /could not download/);
	for (const bad of ["../etc", "1.0/../../x", "-rf", "1.0.0 2"]) {
		const r = get(bad, join(work, "bad", "fenec-server"));
		assert.notEqual(r.status, 0, bad);
		assert.match(r.stderr, /not a release version/, bad);
	}
});
