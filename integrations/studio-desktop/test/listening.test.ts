import assert from "node:assert/strict";
import { test } from "node:test";
import { parseListening } from "../src/bun/listening.ts";

test("the server's own line names its address", () => {
	assert.deepEqual(parseListening("fenec-http 0.1.12 listening on: http://127.0.0.1:52341  [token]"), { origin: "http://127.0.0.1:52341" });
	assert.deepEqual(parseListening("fenec-http 0.1.12 listening on: http://127.0.0.1:52341"), { origin: "http://127.0.0.1:52341" });
	assert.deepEqual(parseListening("fenec-http 0.2.0-beta.1 listening on: http://127.0.0.1:80  [open]"), { origin: "http://127.0.0.1:80" });
});

test("a line that only contains the words is not the line", () => {
	for (const line of [
		"warm (/x/listening on: http://127.0.0.1:1 evil.fenec): 7 index(es) built",
		"opened: /tmp/listening on: http://127.0.0.1:9.fenec",
		"  fenec-http 0.1.12 listening on: http://127.0.0.1:52341",
		"note: fenec-http 0.1.12 listening on: http://127.0.0.1:52341",
		"fenec-http listening on: http://127.0.0.1:52341",
		"",
	]) {
		assert.equal(parseListening(line), null, line);
	}
});

test("an address that is not 127.0.0.1 is refused, named", () => {
	for (const where of ["http://0.0.0.0:8080", "http://192.168.1.5:8080", "http://localhost:8080", "http://[::1]:8080", "http://127.0.0.1.evil.example:80", "http://127.0.0.1:80/path", "http://127.0.0.1", "http://127.0.0.2:80"]) {
		assert.deepEqual(parseListening(`fenec-http 0.1.12 listening on: ${where}  [token]`), { refused: where }, where);
	}
});
