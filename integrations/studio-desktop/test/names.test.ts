import assert from "node:assert/strict";
import { test } from "node:test";
import { collectionStatement, databaseFileName } from "../src/bun/names.ts";

test("a first collection is the one statement that makes it, written as FenecQL writes one", () => {
	assert.deepEqual(collectionStatement("notes", "title text, done bool"), { ok: true, statement: "create collection notes (title text, done bool)" });
	assert.deepEqual(collectionStatement(" tasks ", " title  TEXT @text ,due timestamp, tags [text], at geo, v vector<768>"), {
		ok: true,
		statement: "create collection tasks (title text @text, due timestamp, tags [text], at geo, v vector<768>)",
	});
});

test("what could close the field list and say something else is refused, in words", () => {
	for (const [name, fields] of [
		["notes", "title text); drop collection orders; ("],
		["notes", "title text) --"],
		["notes", "title text, x"],
		["notes", "title"],
		["notes", ""],
		["notes", "title text @hash extra"],
		["notes", "title text @evil"],
		["notes", "ti-tle text"],
		["notes", 'title "text"'],
		["no tes", "title text"],
		["1notes", "title text"],
		["notes;", "title text"],
		["notes", "title text, Title int"],
		["notes", "id text"],
		["notes", "v vector<0>"],
	] as const) {
		const r = collectionStatement(name, fields);
		assert.equal(r.ok, false, `${name} (${fields})`);
		assert.ok(!r.ok && r.message.length > 0);
	}
});

const ok = (typed: string) => {
	const r = databaseFileName(typed);
	assert.ok(r.ok, `${typed}: ${r.ok ? "" : r.message}`);
	return r.fileName;
};
const refused = (typed: string) => {
	const r = databaseFileName(typed);
	assert.equal(r.ok, false, `${JSON.stringify(typed)} should be refused`);
	return r.ok ? "" : r.message;
};

test("a name gets the extension, once", () => {
	assert.equal(ok("orders"), "orders.fenec");
	assert.equal(ok("  orders  "), "orders.fenec");
	assert.equal(ok("orders.fenec"), "orders.fenec");
	assert.equal(ok("Orders.FENEC"), "Orders.FENEC");
	assert.equal(ok("sales 2026"), "sales 2026.fenec");
	assert.equal(ok("müşteri"), "müşteri.fenec");
});

test("nothing that names a folder, a hidden file or no file", () => {
	for (const bad of ["", "   ", "a/b", "../x", "a\\b", "a:b", ".hidden", ".fenec", "..", "tab\tname", "nul\u0000"]) refused(bad);
});

test("a name too long for the file system is refused, one at the limit is not", () => {
	assert.equal(ok("a".repeat(255 - ".fenec".length)), "a".repeat(255 - ".fenec".length) + ".fenec");
	refused("a".repeat(256 - ".fenec".length));
	// 255 bytes, not 255 characters: two-byte letters count twice.
	refused("ş".repeat(130));
});
