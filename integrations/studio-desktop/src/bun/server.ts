// The one place a server, and the `fenec` shell, are started: the bundled
// binaries, run through Bun. What a start does and how a server is watched is
// process.ts, which `node --test` drives the real binary through; the
// arguments are serving.ts's; a backup's run of `fenec` is backups.ts's.

import { PATHS } from "electrobun/main";
import { join } from "node:path";
import type { Run } from "./backups.ts";
import { startServer, type Child, type FileServer, type Spawn } from "./process.ts";
import type { Plan } from "./serving.ts";

export { AlreadyOpen, PortTaken, StartFailed, randomToken, type FileServer } from "./process.ts";

/**
 * The bundled server. Only the test variant lets the environment name another
 * (the checks run the one they built): a shipped app that read it would start
 * whatever program a launching shell named, with the database file and the
 * token. Every environment variable the shipped app reads for itself would be
 * a hook; this is not one there (`checks/release.mjs`).
 */
export function serverBinary(): string {
	const named = __TESTABLE__ ? process.env["FENEC_SERVER_BIN"] : undefined;
	return named || join(PATHS.RESOURCES_FOLDER, "app", "bin", "fenec-server");
}

/** The bundled `fenec` shell, which takes, verifies and restores backups. The same rule as the server's. */
export function cliBinary(): string {
	const named = __TESTABLE__ ? process.env["FENEC_CLI_BIN"] : undefined;
	return named || join(PATHS.RESOURCES_FOLDER, "app", "bin", "fenec");
}

/**
 * Bun's spawn, as process.ts takes one. stdin is a pipe the app holds for the
 * process's life: Bun marks the write end close-on-exec, so no other child
 * inherits it (`checks/orphan.mjs` reads the descriptor tables to see it).
 */
export const bunSpawn: Spawn = (argv, env): Child => {
	const p = Bun.spawn(argv, { env, stdin: "pipe", stdout: "pipe", stderr: "pipe" });
	return {
		pid: p.pid,
		stdin: p.stdin,
		stdout: p.stdout as unknown as AsyncIterable<Uint8Array>,
		stderr: p.stderr as unknown as AsyncIterable<Uint8Array>,
		exited: p.exited.then((c) => c as number | null),
		get exitCode() {
			return p.exitCode;
		},
		kill: (signal) => p.kill(signal),
	};
};

/** A server on `plan`, with `token` its data token. */
export function startFileServer(plan: Plan, token: string): Promise<FileServer> {
	return startServer(bunSpawn, serverBinary(), plan, token);
}

/** What `fenec` writes is kept to this much of each stream: a backup says one line. */
const CLI_OUTPUT = 64 * 1024;
/** A backup of a large database takes a while; one that takes this long is stopped. */
const CLI_TIMEOUT_MS = 30 * 60_000;

/**
 * The bundled `fenec` with `args`, in `env` (backups.ts builds it: the
 * allowlist and the secrets). It ends by itself; its stdin is nothing.
 */
export const runCli: Run = async (args, env) => {
	const p = Bun.spawn([cliBinary(), ...args], { env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
	const read = async (s: ReadableStream<Uint8Array>) => {
		const decoder = new TextDecoder();
		let text = "";
		// Read to the end whatever is kept: a pipe nobody reads would hold the shell up.
		for await (const chunk of s as unknown as AsyncIterable<Uint8Array>) if (text.length < CLI_OUTPUT) text += decoder.decode(chunk, { stream: true });
		return text.slice(0, CLI_OUTPUT);
	};
	const timer = setTimeout(() => p.kill("SIGTERM"), CLI_TIMEOUT_MS);
	const [out, err, code] = await Promise.all([read(p.stdout), read(p.stderr), p.exited]);
	clearTimeout(timer);
	return { code: code as number | null, out, err };
};
