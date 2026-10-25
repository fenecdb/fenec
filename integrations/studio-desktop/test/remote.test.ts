import assert from "node:assert/strict";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { after, test } from "node:test";
import { describeNetworkError, originOf, probeRemote, sendsTokenInTheClear, tokenMayStay } from "../src/bun/remote.ts";

// A stand-in for what a fenec-server answers: the studio's page before any
// token, /_whoami with one. `studio: false` is a server started without
// --studio (or older than it), which answers an unknown path with 404.
function fake(opts: { studio: boolean; token?: string; router?: string }): Promise<{ origin: string; server: Server; seen: string[] }> {
	const seen: string[] = [];
	const server = createServer((req, res) => {
		seen.push(`${req.method} ${req.url} ${req.headers.authorization ?? ""}`);
		const json = (status: number, body: unknown) => {
			res.writeHead(status, { "content-type": "application/json" });
			res.end(JSON.stringify(body));
		};
		if (req.url === "/_studio/") {
			if (!opts.studio) return json(404, { error: "not found" });
			res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
			return void res.end("<!doctype html>");
		}
		const authorized = !opts.token || req.headers.authorization === `Bearer ${opts.token}`;
		const here = opts.router ? `/t/${opts.router}/_whoami` : "/_whoami";
		if (req.url === "/_whoami" && opts.router) return json(404, { error: "use /t/<tenant>/..." });
		if (req.url === here) return authorized ? json(200, { node: "single" }) : json(401, { error: "invalid token" });
		return json(404, { error: "not found" });
	});
	return new Promise((resolve) =>
		server.listen(0, "127.0.0.1", () => resolve({ origin: `http://127.0.0.1:${(server.address() as AddressInfo).port}`, server, seen })),
	);
}

const servers: Server[] = [];
after(() => servers.forEach((s) => s.close()));
const start = async (o: Parameters<typeof fake>[0]) => {
	const f = await fake(o);
	servers.push(f.server);
	return f;
};

test("a server with a studio and the right token is fine", async () => {
	const { origin, seen } = await start({ studio: true, token: "secret" });
	assert.deepEqual(await probeRemote(origin, "secret", null), { kind: "ok" });
	// The token travels in a header, never in a URL.
	assert.ok(seen.every((line) => !line.split(" ")[1]!.includes("secret")), seen.join("\n"));
});

// A server of whatever kind: `route` answers each request, and what was asked of it is kept.
function stand(route: (url: string, authorization: string | undefined, res: import("node:http").ServerResponse) => void): Promise<{ origin: string; seen: string[] }> {
	const seen: string[] = [];
	const server = createServer((req, res) => {
		seen.push(`${req.url} ${req.headers.authorization ?? ""}`.trim());
		route(req.url ?? "", req.headers.authorization, res);
	});
	servers.push(server);
	return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve({ origin: `http://127.0.0.1:${(server.address() as AddressInfo).port}`, seen })));
}
const studioPage = (res: import("node:http").ServerResponse) => {
	res.writeHead(200, { "content-type": "text/html" });
	res.end("<!doctype html>");
};

test("a redirect is not followed, and the token is not sent where it points", async () => {
	const elsewhere = await stand((_, __, res) => res.writeHead(200).end("{}"));
	const server = await stand((url, _, res) => {
		if (url === "/_studio/") return studioPage(res);
		res.writeHead(302, { location: `${elsewhere.origin}/_whoami?next=1` }).end();
	});
	const p = await probeRemote(server.origin, "secret-token", null);
	assert.deepEqual(p, { kind: "redirected", status: 302, to: elsewhere.origin });
	assert.deepEqual(elsewhere.seen, [], "nothing reached the address the redirect named");
	assert.ok(server.seen.every((line) => !line.includes("secret-token")), `the token was not even sent to the server that redirected: ${server.seen.join(" | ")}`);
});

test("a redirect on the studio's own page is a message too", async () => {
	const elsewhere = await stand((_, __, res) => studioPage(res));
	const server = await stand((_, __, res) => res.writeHead(301, { location: `${elsewhere.origin}/_studio/` }).end());
	assert.deepEqual(await probeRemote(server.origin, "t", null), { kind: "redirected", status: 301, to: elsewhere.origin });
	assert.deepEqual(elsewhere.seen, []);
});

test("the token goes only to a server that has said it is fenecdb: its own 401, a JSON body naming an error", async () => {
	// A sign-in page in front, whatever it is: a 401 that is not fenecdb's.
	const proxy = await stand((url, _, res) => (url === "/_studio/" ? studioPage(res) : res.writeHead(401, { "content-type": "text/html" }).end("<h1>sign in</h1>")));
	assert.deepEqual(await probeRemote(proxy.origin, "secret-token", null), { kind: "ok" });
	assert.ok(proxy.seen.every((line) => !line.includes("secret-token")), `never sent: ${proxy.seen.join(" | ")}`);
	// fenecdb's own: asked without, then with.
	const fenec = await start({ studio: true, token: "secret-token" });
	assert.deepEqual(await probeRemote(fenec.origin, "secret-token", null), { kind: "ok" });
	assert.ok(fenec.seen.some((l) => l.includes("/_whoami") && !l.includes("secret-token")), "asked without the token first");
	assert.ok(fenec.seen.some((l) => l.includes("/_whoami") && l.includes("Bearer secret-token")), "and with it after");
});

test("a server that takes no token is not given one", async () => {
	const open = await start({ studio: true });
	assert.deepEqual(await probeRemote(open.origin, "saved-token", null), { kind: "ok" });
	assert.ok(open.seen.every((l) => !l.includes("saved-token")), open.seen.join(" | "));
});

test("a token stays with the address it was saved for", () => {
	assert.equal(tokenMayStay("https://prod.example.com", "https://prod.example.com", ""), true, "the same address, no new token: it stays");
	assert.equal(tokenMayStay("https://prod.example.com", "https://staging.example.com", ""), false, "another address and none typed: it must not");
	assert.equal(tokenMayStay("https://prod.example.com", "https://staging.example.com", "typed"), true, "another address with its own token");
	assert.equal(tokenMayStay("https://prod.example.com", "http://prod.example.com", ""), false, "the scheme is part of it");
	assert.equal(tokenMayStay("https://prod.example.com", "https://prod.example.com:8443", ""), false, "so is the port");
});

test("a host that is a glob is not an address: it would widen the window's navigation rules", () => {
	assert.throws(() => originOf("https://*.example.com"), /cannot hold a \*/);
	assert.throws(() => originOf("http://127.0.0.*:8080"), /cannot hold a \*/);
});

test("a server that does not serve the studio is told apart from one that is down", async () => {
	const { origin } = await start({ studio: false });
	assert.deepEqual(await probeRemote(origin, "x", null), { kind: "no-studio", status: 404 });
});

test("the wrong token and no token, where one is needed, are refused in words", async () => {
	const { origin } = await start({ studio: true, token: "secret" });
	assert.deepEqual(await probeRemote(origin, "wrong", null), { kind: "token-refused", detail: "invalid token", hadToken: true });
	assert.deepEqual(await probeRemote(origin, "", null), { kind: "token-refused", detail: "invalid token", hadToken: false });
});

test("a router answers 404 for /_whoami: the tenant's path is asked, so a wrong token is still found", async () => {
	const { origin } = await start({ studio: true, token: "secret", router: "acme" });
	assert.deepEqual(await probeRemote(origin, "secret", "acme"), { kind: "ok" });
	assert.equal((await probeRemote(origin, "wrong", "acme")).kind, "token-refused");
	// With no tenant named there is nothing to ask a router: the studio itself asks.
	assert.deepEqual(await probeRemote(origin, "wrong", null), { kind: "ok" });
});

test("nothing listening is unreachable, with a reason a person can act on", async () => {
	const { origin, server } = await start({ studio: true });
	await new Promise((r) => server.close(r));
	const p = await probeRemote(origin, "x", null);
	assert.equal(p.kind, "unreachable");
	assert.match((p as { reason: string }).reason, /nothing is listening/);
});

test("a fetch that times out says so", async () => {
	const never = (() => Promise.reject(Object.assign(new Error("The operation timed out."), { name: "TimeoutError" }))) as unknown as typeof fetch;
	const p = await probeRemote("http://10.255.255.1:9", "x", null, never);
	assert.deepEqual(p, { kind: "unreachable", reason: "no answer in 6 seconds" });
});

test("network errors, as Node and Bun spell them", () => {
	const cause = (code: string) => Object.assign(new TypeError("fetch failed"), { cause: { code } });
	assert.match(describeNetworkError(cause("ECONNREFUSED")), /nothing is listening/);
	assert.match(describeNetworkError(cause("ENOTFOUND")), /does not resolve/);
	assert.match(describeNetworkError({ code: "ConnectionRefused" }), /nothing is listening/);
	assert.match(describeNetworkError(cause("CERT_HAS_EXPIRED")), /certificate is not accepted \(CERT_HAS_EXPIRED\)/);
	assert.match(describeNetworkError(cause("UNABLE_TO_VERIFY_LEAF_SIGNATURE")), /certificate/);
	assert.equal(describeNetworkError(new Error("boom")), "boom");
});

test("an address is an origin: a path, a query and a fragment are dropped, a bare host is http", () => {
	assert.equal(originOf("https://db.example.com/some/path?x=1#y"), "https://db.example.com");
	assert.equal(originOf("127.0.0.1:8080"), "http://127.0.0.1:8080");
	assert.equal(originOf("  http://localhost:8080/  "), "http://localhost:8080");
	assert.throws(() => originOf(""), /address/);
	assert.throws(() => originOf("ftp://x"), /http/);
	assert.throws(() => originOf("http://"), /not an address/);
});

test("plain http to another machine sends the token in the clear; this machine and https do not", () => {
	assert.equal(sendsTokenInTheClear("http://db.example.com"), true);
	assert.equal(sendsTokenInTheClear("http://192.168.1.20:8080"), true);
	assert.equal(sendsTokenInTheClear("https://db.example.com"), false);
	assert.equal(sendsTokenInTheClear("http://127.0.0.1:8080"), false);
	assert.equal(sendsTokenInTheClear("http://localhost:8080"), false);
	assert.equal(sendsTokenInTheClear("http://[::1]:8080"), false);
});
