import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
import { kindOfFile, SIGNATURE } from "../src/bun/dbfile.ts";

const dir = mkdtempSync(join(tmpdir(), "fenec-dbfile-"));
after(() => rmSync(dir, { recursive: true, force: true }));
const make = (name: string, bytes: Buffer | string) => {
	const p = join(dir, name);
	writeFileSync(p, bytes);
	return p;
};

test("a file that starts with the signature is a database, whatever follows it", () => {
	assert.equal(SIGNATURE, "FENECDB");
	assert.equal(kindOfFile(make("a.fenec", Buffer.concat([Buffer.from("FENECDB\x01"), Buffer.alloc(100)]))), "database");
	// The eighth byte is the format's version: a newer one is the server's to refuse, in its words.
	assert.equal(kindOfFile(make("newer.fenec", Buffer.concat([Buffer.from("FENECDB\x09"), Buffer.alloc(10)]))), "database");
});

test("an empty file is empty, not a database", () => {
	assert.equal(kindOfFile(make("empty.fenec", "")), "empty");
});

test("anything else is some other kind of file", () => {
	assert.equal(kindOfFile(make("notes.txt", "plain text, not a database\n")), "other");
	assert.equal(kindOfFile(make("short", "FENEC")), "other");
	assert.equal(kindOfFile(make("sqlite", "SQLite format 3\u0000")), "other");
	assert.equal(kindOfFile(make("almost", "fenecdb\x01....")), "other", "the signature's case matters");
});

test("a file that cannot be read says so instead of being called a kind", () => {
	assert.throws(() => kindOfFile(join(dir, "nothing-here")), /ENOENT/);
});
