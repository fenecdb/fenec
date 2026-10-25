// One fenec-server per open file.
//
//   fenec-server --file <path> --studio --http 127.0.0.1:0
//
// with a random token in FENEC_HTTP_TOKEN (the environment, not argv: `ps`
// shows argv to every user), found by the `listening on: http://<address>`
// line on stderr, which is how every harness of the repository finds a server
// that listens on port 0. On a window's close it gets SIGTERM, which syncs and
// writes the checkpoint (the HNSW graphs) on the way down.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { PATHS, Utils } from "electrobun/main";

/** The file is open in another process: the server refused its `<file>.lock`. */
export class AlreadyOpen extends Error {}
/** The server never said where it listens. */
export class StartFailed extends Error {}

export function serverBinary(): string {
	return process.env["FENEC_SERVER_BIN"] || join(PATHS.RESOURCES_FOLDER, "app", "bin", "fenec-server");
}

function randomToken(): string {
	const bytes = crypto.getRandomValues(new Uint8Array(32));
	return Buffer.from(bytes).toString("base64url");
}

export type FileServer = {
	file: string;
	origin: string;
	token: string;
	pid: number;
	/** SIGTERM, then SIGKILL after `graceMs`. Resolves once the process is gone. */
	stop(graceMs?: number): Promise<number | null>;
	/** Resolves when the process exits, for whatever reason. */
	exited: Promise<number | null>;
	/** The last lines it wrote to stderr. */
	log(): string;
};

const LISTENING = /listening on: (http:\/\/\S+)/;
const START_TIMEOUT_MS = 30_000;

export async function startFileServer(file: string): Promise<FileServer> {
	const token = randomToken();
	const proc = Bun.spawn(
		[serverBinary(), "--file", file, "--studio", "--http", "127.0.0.1:0"],
		{
			env: { ...process.env, FENEC_HTTP_TOKEN: token },
			stdin: "ignore",
			stdout: "ignore",
			stderr: "pipe",
		},
	);
	const tail: string[] = [];
	const note = (line: string) => {
		tail.push(line);
		if (tail.length > 200) tail.shift();
	};

	let announce!: (origin: string) => void;
	const listening = new Promise<string>((resolve) => (announce = resolve));
	// Drain stderr for the life of the process: a pipe nobody reads fills up
	// and blocks the server's log writes.
	(async () => {
		const decoder = new TextDecoder();
		let pending = "";
		for await (const chunk of proc.stderr as unknown as AsyncIterable<Uint8Array>) {
			pending += decoder.decode(chunk, { stream: true });
			let i: number;
			while ((i = pending.indexOf("\n")) >= 0) {
				const line = pending.slice(0, i);
				pending = pending.slice(i + 1);
				note(line);
				const m = LISTENING.exec(line);
				if (m) announce(m[1]!);
			}
		}
		if (pending) note(pending);
	})();

	const exited = proc.exited.then((code) => code as number | null);
	const timeout = new Promise<"timeout">((r) => setTimeout(() => r("timeout"), START_TIMEOUT_MS));
	const first = await Promise.race([listening, exited.then(() => "exited" as const), timeout]);
	if (first === "exited" || first === "timeout") {
		if (first === "timeout") proc.kill("SIGKILL");
		await exited;
		// Let the stderr reader see the last line before judging it.
		await Bun.sleep(30);
		const text = tail.join("\n");
		if (text.includes("it is open already")) throw new AlreadyOpen(text);
		throw new StartFailed(text || `fenec-server ${first === "timeout" ? "did not start in time" : "exited"}`);
	}

	remember(proc.pid, file);
	let stopping: Promise<number | null> | null = null;
	return {
		file,
		origin: first,
		token,
		pid: proc.pid,
		exited,
		log: () => tail.join("\n"),
		stop(graceMs = 30_000) {
			stopping ??= (async () => {
				if (proc.exitCode === null) proc.kill("SIGTERM");
				const done = await Promise.race([exited, new Promise<"late">((r) => setTimeout(() => r("late"), graceMs))]);
				if (done === "late") {
					// Not retried by the server and safe for the data (the log is
					// append-only and a torn tail is cut at the next open), but the
					// checkpoint is lost: the graphs are linked again at the next open.
					proc.kill("SIGKILL");
					await exited;
				}
				forget(proc.pid);
				return proc.exitCode;
			})();
			return stopping;
		},
	};
}

// ------------------------------------------------------------------ strays
//
// A killed or crashed app cannot stop its children (macOS has no
// parent-death signal), and an orphaned server keeps `<file>.lock`: the next
// open of that file is refused. So every server started is noted in
// userData/servers.json, and the next launch stops the ones still running
// that are still a fenec-server.

const registry = () => join(Utils.paths.userData, "servers.json");

function readRegistry(): { pid: number; file: string }[] {
	try {
		return JSON.parse(readFileSync(registry(), "utf8"));
	} catch {
		return [];
	}
}

function writeRegistry(list: { pid: number; file: string }[]) {
	mkdirSync(Utils.paths.userData, { recursive: true });
	writeFileSync(registry(), JSON.stringify(list));
}

function remember(pid: number, file: string) {
	writeRegistry([...readRegistry().filter((e) => e.pid !== pid), { pid, file }]);
}

function forget(pid: number) {
	writeRegistry(readRegistry().filter((e) => e.pid !== pid));
}

/** SIGTERM to every server a previous run left behind. Returns the files they held. */
export async function reapStrays(): Promise<string[]> {
	const reaped: string[] = [];
	for (const e of readRegistry()) {
		const ps = Bun.spawnSync(["ps", "-p", String(e.pid), "-o", "comm="]);
		const comm = new TextDecoder().decode(ps.stdout).trim();
		if (comm.endsWith("fenec-server")) {
			process.kill(e.pid, "SIGTERM");
			reaped.push(e.file);
		}
	}
	if (reaped.length) await Bun.sleep(1500);
	if (existsSync(registry())) writeRegistry([]);
	return reaped;
}
