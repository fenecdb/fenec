import assert from "node:assert/strict";
import { test } from "node:test";
import { collectionStatement, databaseFileName, fileNameRefusal } from "../src/bun/names.ts";

test("a first collection is the one statement that makes it, written as FenecQL writes one", () => {
	assert.deepEqual(collectionStatement("notes", "title text, done bool"), { ok: true, statement: "create collection notes (title text, done bool)" });
	assert.deepEqual(collectionStatement(" tasks ", " title  TEXT @text ,due timestamp, tags [text], at geo, v vector<768>"), {
		ok: true,
		statement: "create collection tasks (title text @text, due timestamp, tags [text], at geo, v vector<768>)",
	});
	// An index is a word of the language, which is lower case: typed in capitals it is written as one.
	assert.deepEqual(collectionStatement("notes", "sku text @HASH, at timestamp @Sorted"), { ok: true, statement: "create collection notes (sku text @hash, at timestamp @sorted)" });
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
	for (const bad of ["", "   ", "a/b", "../x", "a\\b", "a:b", ".hidden", ".fenec", "..", "tab\tname", "nul\u0000", "del\u007f"]) refused(bad);
});

test("a name drawn in another order than it is spelled is refused: a database's and a key's", () => {
	// invoice<U+202E>slx.fenec is shown as "invoicenef.xls".
	for (const control of ["\u202e", "\u202d", "\u202a", "\u202b", "\u202c", "\u2066", "\u2067", "\u2068", "\u2069", "\u200e", "\u200f", "\u061c"]) {
		assert.match(refused(`invoice${control}slx`), /direction/, `U+${control.codePointAt(0)!.toString(16)}`);
		assert.match(fileNameRefusal(`backups${control}.key`) ?? "", /direction/);
	}
	// Right-to-left letters themselves are a name like any other.
	assert.equal(ok("\u05e9\u05dc\u05d5\u05dd"), "\u05e9\u05dc\u05d5\u05dd.fenec");
	assert.equal(ok("\u0645\u0631\u062d\u0628\u0627"), "\u0645\u0631\u062d\u0628\u0627.fenec");
});

test("a name too long for the file system is refused, one at the limit is not", () => {
	assert.equal(ok("a".repeat(255 - ".fenec".length)), "a".repeat(255 - ".fenec".length) + ".fenec");
	refused("a".repeat(256 - ".fenec".length));
	// 255 bytes, not 255 characters: two-byte letters count twice.
	refused("ş".repeat(130));
});
