import assert from "node:assert/strict";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { after, test } from "node:test";
import { describeNetworkError, originOf, probeRemote, sendsTokenInTheClear } from "../src/bun/remote.ts";

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
