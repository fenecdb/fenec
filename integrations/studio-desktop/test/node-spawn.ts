// Node's spawn and a runner of `fenec`, as process.ts and backups.ts take them,
// so `node --test` drives the real binaries through the app's own code (the
// app hands in Bun's: server.ts). Not a test file of its own.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import type { Run } from "../src/bun/backups.ts";
import type { Child, Spawn } from "../src/bun/process.ts";

const BIN = new URL("../bin/", import.meta.url).pathname;
/** The binaries `make studio-desktop` puts in bin/ (scripts/get-server.sh). */
export const SERVER = process.env["FENEC_SERVER_BIN"] || `${BIN}fenec-server`;
export const CLI = process.env["FENEC_CLI_BIN"] || `${BIN}fenec`;
/** Why a test that needs them cannot run, or "" when it can. */
export const missing = () => [SERVER, CLI].filter((b) => !existsSync(b)).map((b) => `${b} is not there: run scripts/get-server.sh`).join("; ");

export const nodeSpawn: Spawn = (argv, env): Child => {
	const p = spawn(argv[0]!, argv.slice(1), { env, stdio: ["pipe", "pipe", "pipe"] });
	const exited = new Promise<number | null>((resolve) => p.on("exit", (code) => resolve(code)));
	return {
		pid: p.pid!,
		stdin: p.stdin,
		stdout: p.stdout as unknown as AsyncIterable<Uint8Array>,
		stderr: p.stderr as unknown as AsyncIterable<Uint8Array>,
		exited,
		get exitCode() {
			return p.exitCode;
		},
		kill: (signal) => void p.kill(signal),
	};
};

/** `fenec` with `args`, its output whole. */
export const nodeRun: Run = (args, env) =>
	new Promise((resolve) => {
		const p = spawn(CLI, args, { env, stdio: ["ignore", "pipe", "pipe"] });
		let out = "";
		let err = "";
		p.stdout.on("data", (b) => (out += b));
		p.stderr.on("data", (b) => (err += b));
		p.on("close", (code) => resolve({ code, out, err }));
	});
