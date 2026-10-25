import assert from "node:assert/strict";
import { test } from "node:test";
import vm from "node:vm";
import { sessionPreload } from "../src/bun/session.ts";

// The preload is the one place a token reaches a page, so what it does is
// held here, by running it as the page would: in a context with a `location`
// and a `sessionStorage` and nothing else.
function run(script: string, pageOrigin: string, opts: { storageThrows?: boolean } = {}) {
	const kept = new Map<string, string>();
	const sandbox = {
		location: { origin: pageOrigin },
		sessionStorage: {
			setItem(k: string, v: string) {
				if (opts.storageThrows) throw new Error("denied");
				kept.set(k, v);
			},
		},
	};
	vm.runInNewContext(script, sandbox);
	return kept;
}

const session = { origin: "http://127.0.0.1:52341", token: 'to"ken</script>\\', tenant: null };

test("the studio finds its session where it looks: sessionStorage, key fenec-studio", () => {
	const kept = run(sessionPreload(session), session.origin);
	assert.deepEqual([...kept.keys()], ["fenec-studio"]);
	// What studio/connect.js `saved()` parses: {server, token, tenant}.
	assert.deepEqual(JSON.parse(kept.get("fenec-studio")!), { server: session.origin, token: session.token, tenant: null });
});

test("a tenant goes along", () => {
	const kept = run(sessionPreload({ ...session, tenant: "acme" }), session.origin);
	assert.equal(JSON.parse(kept.get("fenec-studio")!).tenant, "acme");
});

test("a page on any other origin is handed nothing", () => {
	for (const other of ["https://example.com", "http://127.0.0.1:52342", "http://127.0.0.1.evil.example", "https://127.0.0.1:52341", "null", ""]) {
		assert.equal(run(sessionPreload(session), other).size, 0, other);
	}
});

test("the token is data in the script, not code: a token that tries to break out cannot", () => {
	const evil = { ...session, token: `");globalThis.pwned=1;("` };
	const sandbox: Record<string, unknown> = { location: { origin: session.origin }, sessionStorage: { setItem() {} } };
	vm.runInNewContext(sessionPreload(evil), sandbox);
	assert.equal(sandbox["pwned"], undefined);
});

test("a page that cannot store is left alone, no error reaches it", () => {
	assert.doesNotThrow(() => run(sessionPreload(session), session.origin, { storageThrows: true }));
});

test("the script names no URL, query or fragment to carry the token in", () => {
	const script = sessionPreload(session);
	assert.ok(!/location\.(href|search|hash)|history\.|document\.cookie|localStorage/.test(script));
});
