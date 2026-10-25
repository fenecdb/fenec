import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { test } from "node:test";

// electrobun.config.ts is code, read at build time: signing and notarizing
// happen only where their secrets exist, and an empty one is none. Each case
// loads the config in a fresh process with the environment it names.

const dir = new URL("..", import.meta.url).pathname;
const SECRETS = [
	"ELECTROBUN_DEVELOPER_ID",
	"ELECTROBUN_APPLEAPIKEY",
	"ELECTROBUN_APPLEAPIISSUER",
	"ELECTROBUN_APPLEAPIKEYPATH",
	"ELECTROBUN_APPLEID",
	"ELECTROBUN_APPLEIDPASS",
	"ELECTROBUN_TEAMID",
	"FENEC_DESKTOP_TEST_BUILD",
];

function config(env: Record<string, string> = {}) {
	const clean = Object.fromEntries(Object.entries(process.env).filter(([k]) => !SECRETS.includes(k)));
	const r = spawnSync(process.execPath, ["--input-type=module", "-e", "console.log(JSON.stringify((await import('./electrobun.config.ts')).default))"], {
		cwd: dir,
		env: { ...clean, ...env },
		encoding: "utf8",
	});
	assert.equal(r.status, 0, r.stderr);
	return JSON.parse(r.stdout);
}

const apiKey = { ELECTROBUN_APPLEAPIKEY: "K", ELECTROBUN_APPLEAPIISSUER: "I", ELECTROBUN_APPLEAPIKEYPATH: "/tmp/k.p8" };

test("with no secrets the build is unsigned, and still makes a DMG", () => {
	const c = config();
	assert.equal(c.build.mac.codesign, false);
	assert.equal(c.build.mac.notarize, false);
	assert.equal(c.build.mac.createDmg, true);
});

test("a certificate signs; with the App Store Connect key it is notarized too", () => {
	assert.deepEqual(pick(config({ ELECTROBUN_DEVELOPER_ID: "Developer ID Application: X (T)" })), { codesign: true, notarize: false });
	assert.deepEqual(pick(config({ ELECTROBUN_DEVELOPER_ID: "Developer ID Application: X (T)", ...apiKey })), { codesign: true, notarize: true });
});

test("an Apple ID with an app-specific password notarizes as well", () => {
	const id = { ELECTROBUN_DEVELOPER_ID: "D", ELECTROBUN_APPLEID: "a@b.c", ELECTROBUN_APPLEIDPASS: "p", ELECTROBUN_TEAMID: "T" };
	assert.deepEqual(pick(config(id)), { codesign: true, notarize: true });
});

test("a part of a key is no key, and notarizing without signing is not asked for", () => {
	assert.deepEqual(pick(config({ ELECTROBUN_DEVELOPER_ID: "D", ELECTROBUN_APPLEAPIKEY: "K", ELECTROBUN_APPLEAPIISSUER: "I" })), { codesign: true, notarize: false });
	assert.deepEqual(pick(config(apiKey)), { codesign: false, notarize: false });
});

test("a secret that is set but empty, as CI hands one that does not exist, is unset", () => {
	assert.deepEqual(pick(config({ ELECTROBUN_DEVELOPER_ID: "", ...Object.fromEntries(Object.keys(apiKey).map((k) => [k, ""])) })), { codesign: false, notarize: false });
	assert.deepEqual(pick(config({ ELECTROBUN_DEVELOPER_ID: "  " })), { codesign: false, notarize: false });
});

test("the test variant has its own identity and builds, is never signed, and is the only one with the hooks", () => {
	const t = config({ FENEC_DESKTOP_TEST_BUILD: "1", ELECTROBUN_DEVELOPER_ID: "D", ...apiKey });
	const release = config();
	assert.equal(t.app.identifier, "dev.fenecdb.studio.test");
	assert.equal(release.app.identifier, "dev.fenecdb.studio");
	assert.notEqual(t.app.name, release.app.name);
	assert.notEqual(t.build.buildFolder, release.build.buildFolder);
	assert.notEqual(t.build.artifactFolder, release.build.artifactFolder);
	assert.deepEqual(pick(t), { codesign: false, notarize: false });
	assert.equal(t.build.bun.define.__TESTABLE__, "true");
	assert.equal(release.build.bun.define.__TESTABLE__, "false");
	// The Keychain service follows the identifier, so a check never touches a real install's items.
	assert.equal(JSON.parse(t.build.bun.define.__KEYCHAIN_SERVICE__), "dev.fenecdb.studio.test");
	assert.equal(JSON.parse(release.build.bun.define.__KEYCHAIN_SERVICE__), "dev.fenecdb.studio");
});

const pick = (c: any) => ({ codesign: c.build.mac.codesign, notarize: c.build.mac.notarize });
