import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { lstatSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { after, test } from "node:test";
import { claimInstance, instancePath } from "../src/bun/instance.ts";

// One instance of the app at a time: the second asks the first to show itself
// and is told to quit, before it reads state.json or touches the Keychain.

// A short folder: a socket's path is 104 bytes at most on macOS, and the
// test's temporary folder inside $TMPDIR is longer than the app's own name there.
const work = mkdtempSync("/tmp/fdi-");
after(() => rmSync(work, { recursive: true, force: true }));

const asked = () => {
	let n = 0;
	let wake: () => void = () => {};
	return {
		count: () => n,
		onAsk: () => {
			n++;
			wake();
		},
		next: () => new Promise<void>((r) => (wake = r)),
	};
};

test("the first instance claims it; a second is told to quit and the first is asked to show itself", async () => {
	const path = instancePath("one", work);
	const first = asked();
	assert.equal(await claimInstance(path, first.onAsk), true);
	const shown = first.next();
	assert.equal(await claimInstance(path, () => assert.fail("a second instance listens for nobody")), false);
	await shown;
	assert.equal(first.count(), 1);
});

test("two started together: one claims it, the other quits", async () => {
	const path = instancePath("two", work);
	const results = await Promise.all([claimInstance(path, () => {}), claimInstance(path, () => {})]);
	assert.deepEqual(results.sort(), [false, true]);
});

test("a socket an instance that ended left behind is taken over", async () => {
	const path = instancePath("stale", work);
	// A socket file nobody listens on, as a killed instance leaves it: a process
	// that listens and is killed (a listener closed in Node unlinks its socket).
	spawnSync(process.execPath, ["-e", `require("node:net").createServer().listen(${JSON.stringify(path)}, () => process.kill(process.pid, "SIGKILL"))`]);
	assert.ok(lstatSync(path).isSocket(), "the dead instance's socket is there");
	const now = asked();
	assert.equal(await claimInstance(path, now.onAsk), true, "the dead socket answers nothing: this one runs");
	const shown = now.next();
	assert.equal(await claimInstance(path, () => {}), false);
	await shown;
});

test("a plain file where the socket goes is taken over as well", async () => {
	const path = instancePath("file", work);
	writeFileSync(path, "");
	assert.equal(await claimInstance(path, () => {}), true);
});

test("a socket that cannot be made does not keep the app from starting", async () => {
	assert.equal(await claimInstance(join(work, "no-such-folder", "x.instance"), () => {}), true);
	assert.equal(await claimInstance(join(work, "x".repeat(120)), () => {}), true, "a path past the 104 bytes a socket's may have");
});

test("the app's own socket fits where macOS puts the user's temporary folder", () => {
	// $TMPDIR is /var/folders/xx/<28 characters>/T/ for every user.
	const tmp = "/var/folders/8x/24jbdqnn3g3_r2knsn99ntw80000gn/T/";
	assert.ok(Buffer.byteLength(instancePath("dev.fenecdb.studio.test", tmp)) < 104);
});
