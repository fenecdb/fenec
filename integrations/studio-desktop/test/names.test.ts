import assert from "node:assert/strict";
import { test } from "node:test";
import { databaseFileName } from "../src/bun/names.ts";

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
