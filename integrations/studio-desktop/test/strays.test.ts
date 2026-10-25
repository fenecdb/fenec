import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
import { realIo, Strays, type Io } from "../src/bun/strays.ts";

// The reaper against real processes: a script named fenec-server that holds
// the arguments the app starts one with, and does what a server does (ends on
// SIGTERM) or what a hung one does (ignores it).

const dir = mkdtempSync(join(tmpdir(), "fenec-strays-"));
const started: ChildProcess[] = [];
after(() => {
	for (const c of started) c.kill("SIGKILL");
	rmSync(dir, { recursive: true, force: true });
});

const script = (name: string, body: string) => {
	const p = join(dir, name);
	writeFileSync(p, `#!/bin/sh\n${body}\n`);
	chmodSync(p, 0o755);
	return p;
};
const polite = script("fenec-server", "while :; do sleep 0.05; done");
mkdirSync(join(dir, "hung"));
const hung = join(dir, "hung", "fenec-server");
// Ready once the trap is set: `ps` shows the arguments from the moment of the exec, before it.
writeFileSync(hung, "#!/bin/sh\ntrap '' TERM\n: > \"$0.ready\"\nwhile :; do sleep 0.05; done\n");
chmodSync(hung, 0o755);

async function server(bin: string, file: string): Promise<ChildProcess> {
	const child = spawn(bin, ["--file", file, "--studio", "--http", "127.0.0.1:0"], { stdio: "ignore" });
	started.push(child);
	// Alive and showing its arguments to `ps`.
	for (let i = 0; i < 100 && !realIo.isOurs(child.pid!, file); i++) await new Promise((r) => setTimeout(r, 20));
	assert.ok(realIo.isOurs(child.pid!, file), "the stand-in server is up");
	for (let i = 0; i < 100 && bin === hung && !existsSync(`${hung}.ready`); i++) await new Promise((r) => setTimeout(r, 20));
	return child;
}
const registry = () => join(dir, `servers-${Math.random().toString(36).slice(2)}.json`);
const noted = (path: string) => JSON.parse(readFileSync(path, "utf8"));

test("a stray that ends on SIGTERM is waited for, and the registry is emptied", async () => {
	const file = join(dir, "a.fenec");
	const child = await server(polite, file);
	const path = registry();
	const strays = new Strays(path);
	strays.remember(child.pid!, file);
	const done = await strays.reap({ graceMs: 5000, pollMs: 10 });
	// Resolved only once it is gone: the file's lock is free.
	assert.equal(realIo.alive(child.pid!), false);
	assert.deepEqual(done, { reaped: [file], left: [] });
	assert.deepEqual(noted(path), []);
});

test("a stray that ignores SIGTERM is killed after the grace period", async () => {
	const file = join(dir, "b.fenec");
	const child = await server(hung, file);
	const path = registry();
	const strays = new Strays(path);
	strays.remember(child.pid!, file);
	const t0 = Date.now();
	const done = await strays.reap({ graceMs: 300, killMs: 3000, pollMs: 10 });
	assert.ok(Date.now() - t0 >= 300, "it was given its grace first");
	assert.equal(realIo.alive(child.pid!), false);
	assert.deepEqual(done.left, []);
	assert.deepEqual(done.reaped, [file]);
});

test("a pid that is some other program is left alone and dropped from the registry", async () => {
	const other = spawn("sleep", ["30"], { stdio: "ignore" });
	started.push(other);
	const path = registry();
	const strays = new Strays(path);
	strays.remember(other.pid!, join(dir, "c.fenec"));
	const done = await strays.reap({ graceMs: 200, pollMs: 10 });
	assert.equal(realIo.alive(other.pid!), true, "not ours: not signalled");
	assert.deepEqual(done, { reaped: [], left: [] });
	assert.deepEqual(noted(path), []);
});

test("a server of the same file started by hand, without --studio, is not ours", async () => {
	const file = join(dir, "d.fenec");
	const child = spawn(polite, ["--file", file, "--http", "127.0.0.1:0"], { stdio: "ignore" });
	started.push(child);
	await new Promise((r) => setTimeout(r, 150));
	assert.equal(realIo.isOurs(child.pid!, file), false);
});

test("one that survives even SIGKILL stays noted for the next launch", async () => {
	const file = join(dir, "e.fenec");
	const child = await server(polite, file);
	const path = registry();
	// A process the signals do not reach, as one that cannot be killed.
	const deaf: Io = { ...realIo, signal() {} };
	const strays = new Strays(path, deaf);
	strays.remember(child.pid!, file);
	const done = await strays.reap({ graceMs: 100, killMs: 100, pollMs: 10 });
	assert.deepEqual(done.reaped, []);
	assert.deepEqual(done.left, [{ pid: child.pid, file }]);
	assert.deepEqual(noted(path), [{ pid: child.pid, file }], "kept");
	// And the next launch, with working signals, finishes it.
	const again = await new Strays(path).reap({ graceMs: 5000, pollMs: 10 });
	assert.deepEqual(again.reaped, [file]);
	assert.equal(realIo.alive(child.pid!), false);
});

test("the registry is a list of pid and file, written when a server starts and forgotten when it ends", () => {
	const path = registry();
	const strays = new Strays(path);
	strays.remember(11, "/a");
	strays.remember(12, "/b");
	strays.remember(11, "/a");
	assert.deepEqual(noted(path), [
		{ pid: 12, file: "/b" },
		{ pid: 11, file: "/a" },
	]);
	strays.forget(12);
	assert.deepEqual(noted(path), [{ pid: 11, file: "/a" }]);
	assert.ok(existsSync(path));
});
