import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { sessionPreload } from "../src/bun/session.ts";

// The preload is the one place a token reaches a page, so what it does is
// held here by running it as a page would, and then by asking the studio's own
// code (studio/connect.js, the file the studio is served from) what it makes of
// the result. Each case is a process of its own: the property the preload
// defines cannot be taken away, which is the point of it.

const connect = pathToFileURL(fileURLToPath(new URL("../../../studio/connect.js", import.meta.url))).href;

// connect.js imports ./client.js, which the studio build makes and the
// repository does not hold: a stand-in with the one name it takes.
const hooks = `
export async function resolve(specifier, context, next) {
  if (specifier === "./client.js") return { url: "data:text/javascript,export const connect = () => ({})", shortCircuit: true };
  return next(specifier, context);
}`;

type Seen = {
	hosted: boolean;
	saved: { server: string; token: string; tenant: string | null } | null;
	host: null | { frozen: boolean; sessionFrozen: boolean; descriptor: { writable: boolean; enumerable: boolean; configurable: boolean }; prototypeIsObject: boolean };
	stored: string[];
	replaceThrows: boolean;
	mutateHolds: boolean;
	ran: boolean;
};

/** `script` run in a page at `pageOrigin`, then the studio's `saved()`. */
function inPage(script: string, pageOrigin: string): Seen {
	const program = `
import { register } from "node:module";
import vm from "node:vm";
register("data:text/javascript," + encodeURIComponent(${JSON.stringify(hooks)}), import.meta.url);
const stored = [];
globalThis.window = globalThis;
globalThis.location = { origin: ${JSON.stringify(pageOrigin)} };
globalThis.sessionStorage = { getItem: () => null, setItem: (k) => stored.push(k), removeItem: () => {} };
vm.runInThisContext(${JSON.stringify(script)});
const c = await import(${JSON.stringify(connect)});
const h = globalThis.fenecHost;
const d = h && Object.getOwnPropertyDescriptor(globalThis, "fenecHost");
let replaceThrows = false;
try { Object.defineProperty(globalThis, "fenecHost", { value: {} }); } catch { replaceThrows = true; }
let mutateHolds = true;
try { if (h) { h.session.token = "stolen"; mutateHolds = h.session.token !== "stolen"; } } catch { mutateHolds = true; }
c.save({ server: "x", token: "t", tenant: null });
console.log(JSON.stringify({
  hosted: c.hosted(),
  saved: c.saved(),
  host: h ? { frozen: Object.isFrozen(h), sessionFrozen: Object.isFrozen(h.session), descriptor: { writable: d.writable, enumerable: d.enumerable, configurable: d.configurable }, prototypeIsObject: Object.getPrototypeOf(h) === Object.prototype } : null,
  stored,
  replaceThrows,
  mutateHolds,
  ran: typeof globalThis.pwned !== "undefined",
}));`;
	const r = spawnSync(process.execPath, ["--input-type=module", "-e", program], { encoding: "utf8" });
	assert.equal(r.status, 0, r.stderr);
	return JSON.parse(r.stdout.trim().split("\n").pop()!);
}

const session = { origin: "http://127.0.0.1:52341", token: 'to"ken</script>\\ `${x}', tenant: null };

test("the studio takes what the preload defines: frozen, on its own origin, the token as given", () => {
	const seen = inPage(sessionPreload(session), session.origin);
	assert.equal(seen.hosted, true);
	assert.deepEqual(seen.saved, { server: session.origin, token: session.token, tenant: null });
	assert.deepEqual(seen.host, {
		frozen: true,
		sessionFrozen: true,
		descriptor: { writable: false, enumerable: false, configurable: false },
		prototypeIsObject: true,
	});
});

test("a tenant goes along", () => {
	const seen = inPage(sessionPreload({ ...session, tenant: "acme" }), session.origin);
	assert.equal(seen.saved?.tenant, "acme");
});

test("the token is never stored: the studio, under a host, writes nothing to session storage", () => {
	assert.deepEqual(inPage(sessionPreload(session), session.origin).stored, []);
});

test("a page that is not the server's gets nothing: any other origin, a lookalike, another scheme or port", () => {
	for (const other of ["https://example.com", "http://127.0.0.1:52342", "http://127.0.0.1.evil.example", "https://127.0.0.1:52341", "null", ""]) {
		const seen = inPage(sessionPreload(session), other);
		assert.equal(seen.hosted, false, other);
		assert.equal(seen.host, null, other);
		assert.equal(seen.saved, null, `${other}: nothing the studio could send a token from`);
	}
});

test("what was handed over cannot be taken back or changed by the page", () => {
	const seen = inPage(sessionPreload(session), session.origin);
	assert.equal(seen.replaceThrows, true, "the property cannot be redefined");
	assert.equal(seen.mutateHolds, true, "the session cannot be rewritten");
});

test("a token that tries to break out of the script cannot", () => {
	const evil = { ...session, token: `");globalThis.pwned=1;("` };
	const seen = inPage(sessionPreload(evil), session.origin);
	assert.equal(seen.saved?.token, evil.token, "the token, exactly");
	assert.equal(seen.ran, false, "and nothing it said ran");
});

test("a page that cannot define the property is left alone, no error reaches it", () => {
	// The studio then says it was handed nothing, which is true.
	const seen = inPage(`Object.defineProperty(globalThis, "fenecHost", { value: "taken" });${sessionPreload(session)}`, session.origin);
	assert.equal(seen.hosted, false, "a host that is not an object is no host");
});

test("the script names no URL, query, fragment or storage to carry the token in", () => {
	const script = sessionPreload(session);
	assert.ok(!/location\.(href|search|hash)|history\.|document\.cookie|localStorage|sessionStorage/.test(script));
});
