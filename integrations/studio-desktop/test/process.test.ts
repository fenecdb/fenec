import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
import { AlreadyOpen, PortTaken, randomToken, startServer, type Child, type FileServer, type Spawn } from "../src/bun/process.ts";
import { DEFAULT_SETTINGS, servePlan, taskPlan, windowPlan, type Plan } from "../src/bun/serving.ts";
import { missing, nodeSpawn, SERVER } from "./node-spawn.ts";

// The real fenec-server, started through the app's own start (process.ts) on
// port 0, as a window's and a served database's servers are: where it listens,
// that the token is in its environment and not its arguments, that a stop is
// the server's own shutdown (exit 0, the lock let go), and that a file another
// server holds, and a port another program holds, are said as what they are.

const skip = missing() || false;
const work = mkdtempSync(join(tmpdir(), "fenec-desktop-process-"));
const running: FileServer[] = [];
after(async () => {
	await Promise.all(running.map((s) => s.stop()));
	rmSync(work, { recursive: true, force: true });
});

const start = async (plan: Plan, token: string) => {
	const s = await startServer(nodeSpawn, SERVER, plan, token);
	running.push(s);
	return s;
};
/** A database file made by a server, as New database makes one. */
async function database(name: string) {
	const file = join(work, `${name}.fenec`);
	writeFileSync(file, "");
	const s = await start(taskPlan(file), randomToken());
	assert.equal(await s.stop(), 0);
	return file;
}
const argvOf = (pid: number) => execFileSync("ps", ["-ww", "-o", "command=", "-p", String(pid)]).toString();
const status = async (url: string, token?: string) => (await fetch(url, { headers: token ? { authorization: `Bearer ${token}` } : {} })).status;

test("a window's server: loopback, any port, the token in its environment and in no argument, and a stop that is its shutdown", { skip }, async () => {
	const file = await database("window");
	const token = randomToken();
	const s = await start(windowPlan(file, "250", true), token);
	assert.match(s.origin, /^http:\/\/127\.0\.0\.1:\d+$/);
	assert.equal(s.local, s.origin);
	assert.ok(s.replicationToken, "a replication token for backups, which are set up");
	// Which turns the change feed on, to the data token: why only a file with backups gets one.
	assert.equal(await status(`${s.local}/_changes`, token), 200);
	const argv = argvOf(s.pid);
	assert.ok(argv.includes("--exit-on-stdin-close") && argv.includes("--studio"), argv);
	assert.ok(!argv.includes(token) && !argv.includes(s.replicationToken!), "no token on the command line");
	assert.equal(await status(`${s.local}/_whoami`, token), 200);
	assert.equal(await status(`${s.local}/_whoami`), 401);
	assert.equal(await status(`${s.local}/_whoami`, "not-the-token"), 401);
	assert.match(s.log(), /listening on: http:\/\/127\.0\.0\.1/);
	assert.equal(await s.stop(), 0, "SIGTERM: the server's own shutdown, exit 0");
	// The lock went with it: another server takes the file at once.
	const again = await start(windowPlan(file, "250"), randomToken());
	assert.equal(again.replicationToken, null, "no backups set up: no stream, and no /_changes");
	// "this server keeps no change feed".
	assert.equal(await status(`${again.local}/_changes`, again.token), 409);
	assert.equal(await again.stop(), 0);
});

test("under --sync off a server is given no replication token, which it would refuse", { skip }, async () => {
	const file = await database("off");
	const s = await start(windowPlan(file, "off", true), randomToken());
	assert.equal(s.replicationToken, null);
	assert.ok(argvOf(s.pid).includes("--sync off"));
	await s.stop();
});

test("a file another server holds is AlreadyOpen, and nothing of the second is left running", { skip }, async () => {
	const file = await database("held");
	const holder = await start(windowPlan(file, "250"), randomToken());
	await assert.rejects(start(windowPlan(file, "250"), randomToken()), (e) => e instanceof AlreadyOpen && /open already/.test(e.message));
	await holder.stop();
});

test("a served database: the port asked for, on the network's address, its metrics on a port of their own", { skip }, async () => {
	const file = await database("served");
	// A port nothing holds now, as the serve sheet asks for one.
	const port = await new Promise<number>((resolve) => {
		const probe = createServer().listen(0, "127.0.0.1", () => {
			const p = (probe.address() as { port: number }).port;
			probe.close(() => resolve(p));
		});
	});
	const token = randomToken();
	const s = await start(servePlan(file, "always", { ...DEFAULT_SETTINGS, port, reach: "lan", metrics: true, mcp: true, compact: false }), token);
	assert.equal(s.origin, `http://0.0.0.0:${port}`);
	assert.equal(s.local, `http://127.0.0.1:${port}`, "the app and its windows reach it on loopback");
	assert.match(s.metrics ?? "", /^http:\/\/127\.0\.0\.1:\d+$/);
	assert.equal(await status(`${s.metrics}/_metrics`, token), 200);
	const argv = argvOf(s.pid);
	for (const flag of ["--sync always", "--auto-compact off", "--mcp", "--metrics 127.0.0.1:0", `--http 0.0.0.0:${port}`]) assert.ok(argv.includes(flag), `${flag} in ${argv}`);
	assert.equal(await status(`${s.local}/_whoami`, token), 200, "the data endpoint answers with its token");

	// The same port again, on 127.0.0.1: macOS would let it bind beside 0.0.0.0, and take loopback from the first.
	await assert.rejects(start(servePlan(await database("other"), "250", { ...DEFAULT_SETTINGS, port }), randomToken()), (e) => e instanceof PortTaken && e.port === port, "the same port again: taken");
	assert.equal(await s.stop(), 0);
});

test("a port another program answers on at 127.0.0.1 is taken, though 0.0.0.0 would bind beside it", { skip }, async () => {
	const other = createServer((c) => c.end());
	const port = await new Promise<number>((resolve) => other.listen(0, "127.0.0.1", () => resolve((other.address() as { port: number }).port)));
	try {
		await assert.rejects(start(servePlan(await database("beside"), "250", { ...DEFAULT_SETTINGS, port, reach: "lan" }), randomToken()), (e) => e instanceof PortTaken, "the token would have gone to the other program");
	} finally {
		other.close();
	}
});

test("what it writes is kept in a bounded ring, a line an entry, and told to whoever listens", { skip }, async () => {
	const file = await database("log");
	const s = await start(windowPlan(file, "250"), randomToken());
	const seen: string[] = [];
	const stop = s.onLine((l) => seen.push(l));
	assert.ok(s.lines().some((l) => /^fenec-http \S+ listening on:/.test(l)));
	await s.stop();
	stop();
	assert.ok(seen.length > 0 || s.lines().length > 0);
});

/** A stand-in server that writes `lines` on stderr and runs until it is stopped. */
const writes =
	(lines: string[]): Spawn =>
	(): Child => {
		let code: number | null = null;
		let end!: (c: number | null) => void;
		const exited = new Promise<number | null>((r) => (end = r));
		const stderr = (async function* () {
			for (const l of lines) yield new TextEncoder().encode(`${l}\n`);
			await exited;
		})();
		const stdout = (async function* () {
			await exited;
		})();
		return {
			pid: 0,
			stdin: null,
			stdout,
			stderr,
			exited,
			get exitCode() {
				return code;
			},
			kill: () => end((code = 0)),
		};
	};

test("where it listens and its metrics are read before it answers anything: a line after is the data's", async () => {
	const plan = { ...servePlan("/nowhere.fenec", "250", { ...DEFAULT_SETTINGS, metrics: true }), port: 0 };
	// A statement logged after the start can hold a newline and so a line of its own.
	const later = ["metrics on: http://127.0.0.1:6666/_metrics  [token]", "fenec-http 0.0.0 listening on: http://127.0.0.1:7777  [x]"];
	const s = await startServer(writes(["fenec-http 0.0.0 listening on: http://127.0.0.1:5555  [token required]", ...later]), "fenec-server", plan, randomToken());
	assert.equal(s.port, 5555);
	assert.equal(s.metrics, null, "none was said before it listened, and none is taken after");
	await s.stop();
	const t = await startServer(writes(["metrics on: http://127.0.0.1:9101/_metrics  [token]", "fenec-http 0.0.0 listening on: http://127.0.0.1:5556  [x]", ...later]), "fenec-server", plan, randomToken());
	assert.equal(t.metrics, "http://127.0.0.1:9101");
	assert.equal(t.port, 5556);
	await t.stop();
	// Not asked for, not read: a metrics line from a server started without them is nobody's.
	const u = await startServer(writes(["metrics on: http://127.0.0.1:9102/_metrics  [token]", "fenec-http 0.0.0 listening on: http://127.0.0.1:5557  [x]"]), "fenec-server", { ...plan, metrics: false }, randomToken());
	assert.equal(u.metrics, null);
	await u.stop();
});
