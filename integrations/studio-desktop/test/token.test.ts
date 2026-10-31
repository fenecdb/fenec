import assert from "node:assert/strict";
import { test } from "node:test";
import { keychainName, sealToken, tokenFor } from "../src/bun/token.ts";

const prod = "https://prod.example.com";
const staging = "https://staging.example.com";

test("a token comes back for the address it was saved for, and only that", () => {
	const item = sealToken(prod, "s3cret");
	assert.equal(tokenFor(item, prod), "s3cret");
	assert.equal(tokenFor(item, staging), "", "another host");
	assert.equal(tokenFor(item, "http://prod.example.com"), "", "another scheme");
	assert.equal(tokenFor(item, "https://prod.example.com:8443"), "", "another port");
	assert.equal(tokenFor(item, ""), "");
});

test("whatever is not what this app writes is no token for any address", () => {
	for (const item of [null, undefined, "", "a bare token", "{}", "[]", "null", '{"origin":"x"}', '{"token":"t"}', '{"origin":"https://prod.example.com","token":5}', "{not json"]) {
		assert.equal(tokenFor(item as string | null | undefined, prod), "", String(item));
	}
});

test("a token with any characters survives the round trip", () => {
	const token = 'to"ken\n</script>\\ `${x}';
	assert.equal(tokenFor(sealToken(prod, token), prod), token);
});

test("an item names its connection", () => {
	assert.equal(keychainName("abc"), "connection:abc");
});
