import assert from "node:assert/strict";
import { test } from "node:test";
import { commandShown, DEFAULT_SETTINGS, factsOf, parsePort, serverArgs, servePlan, settingsFrom, snippets, taskPlan, windowPlan } from "../src/bun/serving.ts";

const FILE = "/Users/me/Data/shop.fenec";

test("a window's server: this Mac, any port, the studio, and its stdin watched", () => {
	assert.deepEqual(serverArgs(windowPlan(FILE, "250")), ["--file", FILE, "--http", "127.0.0.1:0", "--sync", "250", "--studio", "--replication-buffer", "1", "--exit-on-stdin-close"]);
});

test("a served database's arguments are what the sheet chose, and every server watches its stdin last", () => {
	const lan = serverArgs(servePlan(FILE, "always", { port: 8080, reach: "lan", studio: false, compact: false, mcp: true, metrics: true }));
	assert.deepEqual(lan, ["--file", FILE, "--http", "0.0.0.0:8080", "--sync", "always", "--auto-compact", "off", "--mcp", "--metrics", "127.0.0.1:0", "--replication-buffer", "1", "--exit-on-stdin-close"]);
	const mac = serverArgs(servePlan(FILE, "250", DEFAULT_SETTINGS));
	assert.deepEqual(mac.slice(0, 7), ["--file", FILE, "--http", "127.0.0.1:0", "--sync", "250", "--studio"]);
	assert.ok(!mac.includes("--auto-compact"), "the server's own default: said only to turn it off");
	assert.equal(mac.at(-1), "--exit-on-stdin-close");
});

test("no secret is ever an argument; under --sync off there is no replication either", () => {
	for (const plan of [windowPlan(FILE, "250"), servePlan(FILE, "always", DEFAULT_SETTINGS), taskPlan(FILE), windowPlan(FILE, "off")]) {
		const args = serverArgs(plan).join(" ");
		assert.ok(!/token|secret|key/i.test(args), args);
	}
	assert.equal(windowPlan(FILE, "off").replication, false);
	assert.ok(!serverArgs(servePlan(FILE, "off", DEFAULT_SETTINGS)).includes("--replication-buffer"));
	// A server for a backup alone syncs: it takes no write of its own, and a backup needs one that serves the stream.
	assert.equal(taskPlan(FILE).replication, true);
	assert.ok(!serverArgs(taskPlan(FILE)).includes("--studio"));
});

test("the command the sheet shows has its secrets as dots and the home folder as ~", () => {
	const shown = commandShown(servePlan(FILE, "250", { ...DEFAULT_SETTINGS, port: 8080 }), "/Users/me");
	assert.match(shown, /^FENEC_HTTP_TOKEN=•••• FENEC_REPLICATION_TOKEN=•••• fenec-server --file ~\/Data\/shop\.fenec \\\n {2}--http 127\.0\.0\.1:8080 \\\n {2}--sync 250/);
	assert.ok(shown.endsWith("--exit-on-stdin-close"));
	assert.match(commandShown(servePlan("/Users/me/My Data/it's.fenec", "off", DEFAULT_SETTINGS), "/Users/me"), /^FENEC_HTTP_TOKEN=•••• fenec-server --file '~\/My Data\/it'\\''s\.fenec'/);
});

test("a port as typed: empty is any, a number is one, anything else is said", () => {
	assert.deepEqual(parsePort(""), { ok: true, port: null });
	assert.deepEqual(parsePort(" 8080 "), { ok: true, port: 8080 });
	for (const bad of ["0", "65536", "80a", "-1", "8080.5", "1e3"]) assert.equal(parsePort(bad).ok, false, bad);
});

test("settings read back from state.json are settings, whatever was there", () => {
	assert.deepEqual(settingsFrom(undefined), DEFAULT_SETTINGS);
	assert.deepEqual(settingsFrom({ port: 8080, reach: "lan", studio: false, compact: true, mcp: true, metrics: false }), { port: 8080, reach: "lan", studio: false, compact: true, mcp: true, metrics: false });
	assert.deepEqual(settingsFrom({ port: "8080", reach: "everyone", studio: "yes" }), DEFAULT_SETTINGS);
	assert.equal(settingsFrom({ port: 70000 }).port, null);
});

test("what a served database's card says", () => {
	assert.equal(factsOf(DEFAULT_SETTINGS, "250"), "This Mac only · token required · synced every 250 ms · studio on · compacts by itself");
	assert.equal(factsOf({ ...DEFAULT_SETTINGS, reach: "lan", mcp: true }, "always"), "Your network · token required · every write synced · studio on · compacts by itself · MCP on");
});

test("the snippets an app connects with carry the address and never the token", () => {
	const s = snippets("http://127.0.0.1:8080", "notes");
	for (const text of Object.values(s)) assert.ok(text.includes("http://127.0.0.1:8080"), text);
	assert.match(s.curl, /Bearer \$FENEC_TOKEN/);
	assert.match(s.curl, /get notes limit 5/);
	assert.match(s.js, /from '@fenecdb\/web\/client'/);
	assert.match(s.js, /process\.env\.FENEC_TOKEN/);
	assert.match(s.py, /from fenecdb import Client/);
	assert.match(s.env, /^FENEC_URL=http:\/\/127\.0\.0\.1:8080\nFENEC_TOKEN=<copy it from Settings>$/);
});
