import assert from "node:assert/strict";
import { test } from "node:test";
import { alreadyHereMessage, diedNotice, emptyFile, heldNotice, missingFile, notADatabase, outOfStepNotice, remoteNotice, startFailedNotice } from "../src/bun/messages.ts";
import type { Probe } from "../src/bun/remote.ts";

const origin = "https://db.example.com";
const failures: Exclude<Probe, { kind: "ok" }>[] = [
	{ kind: "unreachable", reason: "nothing is listening at that address" },
	{ kind: "no-studio", status: 404 },
	{ kind: "token-refused", detail: "invalid token", hadToken: true },
	{ kind: "token-refused", detail: "invalid token", hadToken: false },
	{ kind: "redirected", status: 302, to: "https://login.example.com" },
];

test("a file another program holds says whose it is and what to do", () => {
	const n = heldNotice("orders.fenec", "could not open /x/orders.fenec: it is open already, in this process or another");
	assert.match(n.title, /open in another program/);
	assert.match(n.message, /orders\.fenec/);
	assert.match(n.hint ?? "", /corrupt/);
	assert.match(n.detail ?? "", /it is open already/);
	assert.deepEqual(n.actions?.map((a) => a.id), ["retry"]);
});

test("a server that died names its exit and keeps what it said", () => {
	assert.match(diedNotice("orders.fenec", 137, "boom").message, /code 137/);
	assert.match(diedNotice("orders.fenec", null, "").message, /signal/);
	assert.equal(diedNotice("orders.fenec", 1, "a\nb\nc\nd\ne\nf\ng\nh\ni\nj").detail, "c\nd\ne\nf\ng\nh\ni\nj");
	assert.deepEqual(diedNotice("orders.fenec", 1, "").actions?.map((a) => a.id), ["retry"]);
});

test("each way a saved server can fail has its own words and a way forward", () => {
	const titles = failures.map((p) => remoteNotice("prod", origin, p).title);
	assert.equal(new Set(titles).size, titles.length, titles.join(" | "));
	for (const p of failures) {
		const n = remoteNotice("prod", origin, p);
		assert.match(n.message, /prod/);
		assert.match(n.message, /db\.example\.com/);
		assert.ok(n.hint, `${p.kind} says what to do`);
	}
	assert.match(remoteNotice("prod", origin, failures[1]!).hint!, /--studio/);
	assert.match(remoteNotice("prod", origin, failures[0]!).message, /nothing is listening/);
});

test("a redirect names where it points and says the token is not sent there", () => {
	const n = remoteNotice("prod", origin, failures[4]!);
	assert.match(n.message, /redirect \(HTTP 302\) to https:\/\/login\.example\.com/);
	assert.match(n.hint ?? "", /only to the address it was saved for/);
	assert.deepEqual(n.actions?.map((a) => a.id), ["edit"]);
});

test("a file picked by mistake is told what it is, before any server is started on it", () => {
	assert.match(emptyFile("a.fenec").message, /nothing in it/);
	assert.match(emptyFile("a.fenec").hint ?? "", /New Database/);
	const other = notADatabase("notes.txt");
	assert.match(other.title, /not a fenecdb database/);
	assert.match(other.hint ?? "", /nothing was written beside it/i);
	assert.equal(other.actions, undefined);
});

test("a refused token and a missing one are not the same message, and both offer the form", () => {
	const had = remoteNotice("prod", origin, failures[2]!);
	const none = remoteNotice("prod", origin, failures[3]!);
	assert.match(had.message, /did not accept the token/);
	assert.match(none.message, /none is saved/);
	for (const n of [had, none]) assert.deepEqual(n.actions?.map((a) => a.id), ["edit"]);
});

test("a file that is some other kind of file says so, and that nothing was changed", () => {
	const n = startFailedNotice("notes.txt", "could not open /x/notes.txt: corrupt: invalid fenecdb signature");
	assert.match(n.title, /not a fenecdb database/);
	assert.match(n.hint ?? "", /Nothing was changed/);
	assert.equal(n.actions, undefined, "trying again would not help");
});

test("a server that refuses the app's flags says the app and its server are out of step, and what to do", () => {
	const n = outOfStepNotice("a.fenec", "unknown option: --exit-on-stdin-close\n\nusage: fenec-server ...");
	assert.match(n.title, /out of step/);
	assert.match(n.hint ?? "", /--exit-on-stdin-close/);
	assert.match(n.hint ?? "", /0\.1\.12/);
	assert.match(n.detail ?? "", /unknown option/);
	assert.equal(n.actions, undefined, "trying again would not help");
});

test("the rest", () => {
	assert.match(startFailedNotice("a.fenec", "bad magic").detail ?? "", /bad magic/);
	assert.match(missingFile("/gone.fenec").message, /gone\.fenec/);
	assert.match(alreadyHereMessage("a.fenec"), /already open in a window of this app/);
});
