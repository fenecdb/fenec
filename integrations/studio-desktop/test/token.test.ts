import assert from "node:assert/strict";
import { test } from "node:test";
import { keychainName, sealToken, serveItemName, servedToken, tokenFor, type KeyStore } from "../src/bun/token.ts";

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
	assert.equal(serveItemName("abc"), "serve:abc");
});

/** The Keychain as a map, which a test can look into. */
function keychain() {
	const items = new Map<string, string>();
	const keys: KeyStore = {
		get: async (n) => items.get(n) ?? null,
		set: async (n, v) => void items.set(n, v),
		delete: async (n) => void items.delete(n),
	};
	return { items, keys };
}

test("a served database's token is made once, kept before it is used, and the same at every start", async () => {
	const { items, keys } = keychain();
	let made = 0;
	const make = () => `token-${++made}`;
	const first = await servedToken(keys, "db1", "/Users/me/shop.fenec", make);
	assert.equal(first, "token-1");
	assert.equal(items.get("serve:db1"), sealToken("file:/Users/me/shop.fenec", "token-1"), "kept for its file, before any server has it");
	assert.equal(await servedToken(keys, "db1", "/Users/me/shop.fenec", make), "token-1", "the apps that hold it keep working");
	assert.equal(made, 1);
});

test("New token replaces it; a file moved is another file, and gets a token of its own", async () => {
	const { keys } = keychain();
	let made = 0;
	const make = () => `token-${++made}`;
	await servedToken(keys, "db1", "/a/shop.fenec", make);
	assert.equal(await servedToken(keys, "db1", "/a/shop.fenec", make, true), "token-2");
	assert.equal(await servedToken(keys, "db1", "/a/shop.fenec", make), "token-2");
	assert.equal(await servedToken(keys, "db1", "/b/shop.fenec", make), "token-3", "an item for another path is none");
});

test("a Keychain that refuses the token: no token comes back, so no server is started with one it does not hold", async () => {
	const keys: KeyStore = { get: async () => null, set: async () => Promise.reject(new Error("denied")), delete: async () => {} };
	await assert.rejects(servedToken(keys, "db1", "/a/shop.fenec", () => "t"), /denied/);
});
