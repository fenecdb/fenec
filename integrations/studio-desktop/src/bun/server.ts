// One fenec-server per open file.
//
//   fenec-server --file <path> --studio --http 127.0.0.1:0
//
// with a random token in FENEC_HTTP_TOKEN (the environment, not argv: `ps`
// shows argv to every user), found by the `listening on: http://<address>`
// line on stderr, which is how every harness of the repository finds a server
// that listens on port 0. On a window's close it gets SIGTERM, which syncs and
// writes the checkpoint (the HNSW graphs) on the way down.
//
// `spawnServer` is the one place a server is started. When fenec-server
// learns to exit when its stdin closes (`--exit-on-stdin-close`), that is
// where the flag goes and the stdin becomes a pipe this process holds open,
// and strays.ts goes away.

import { PATHS } from "electrobun/main";
import { join } from "node:path";
import { forget, remember } from "./strays.ts";

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

/** The server's environment: ours without any FENEC_* of the user's (a stray FENEC_JWT_SECRET would change what the server accepts), and the token. */
function serverEnv(token: string): Record<string, string> {
	const env: Record<string, string> = {};
	for (const [k, v] of Object.entries(process.env)) if (v !== undefined && !k.startsWith("FENEC_")) env[k] = v;
	env["FENEC_HTTP_TOKEN"] = token;
	return env;
}

function spawnServer(file: string, token: string) {
	return Bun.spawn([serverBinary(), "--file", file, "--studio", "--http", "127.0.0.1:0"], {
		env: serverEnv(token),
		stdin: "ignore",
		stdout: "ignore",
		stderr: "pipe",
	});
}

export async function startFileServer(file: string): Promise<FileServer> {
	const token = randomToken();
	const proc = spawnServer(file, token);
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
	void exited.then(() => forget(proc.pid));
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
