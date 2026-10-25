// One fenec-server per open file.
//
//   fenec-server --file <path> --studio --http 127.0.0.1:0 --exit-on-stdin-close
//
// with a random token in FENEC_HTTP_TOKEN (the environment, not argv: `ps`
// shows argv to every user), found by the `listening on: http://<address>`
// line on stderr, which is how every harness of the repository finds a server
// that listens on port 0.
//
// Its stdin is a pipe this process holds open for as long as the server runs
// (`FileServer.stdin`). A process that is killed sends its children no signal,
// but the system closes every descriptor it held, so the server sees the pipe
// end however the app ended -- `kill -9` too -- and shuts down as on SIGTERM:
// sync, checkpoint, `<file>.lock` let go, exit 0. That is why no server is left
// holding a file with nobody to answer. On a window's close it is stopped the
// same way a signal does it: SIGTERM.
//
// `spawnServer` is the one place a server is started. Two things about it are
// not obvious. The pipe's write end must stay in this process alone: a child
// that inherited it (a descriptor not marked close-on-exec) would hold the
// server's stdin open after this process has gone, and `checks/orphan.mjs`
// kills the app with two servers up to see both end. And stderr is read for
// the server's whole life, since a pipe nobody reads fills, and the server's
// next log line waits for room.

import { PATHS } from "electrobun/main";
import { join } from "node:path";
import { serverEnvironment } from "./env.ts";
import { parseListening } from "./listening.ts";

/** The file is open in another process: the server refused its `<file>.lock`. */
export class AlreadyOpen extends Error {}
/**
 * The server never said where it listens. `exit` is how it ended, the codes
 * site/content/docs/server.html lists: 1, it could not start (its file could
 * not be opened, which a held lock is the usual reason for, and is
 * AlreadyOpen); 2, a flag was refused -- here, a server that does not know
 * what the app asks of it. `null`: killed, or never ended in time.
 */
export class StartFailed extends Error {
	exit: number | null;
	constructor(message: string, exit: number | null) {
		super(message);
		this.exit = exit;
	}
}

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

function randomToken(): string {
	const bytes = crypto.getRandomValues(new Uint8Array(32));
	return Buffer.from(bytes).toString("base64url");
}

export type FileServer = {
	file: string;
	/** The pipe on its stdin, which the server watches for its end; kept so it is not let go while the server runs. */
	stdin: unknown;
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

const START_TIMEOUT_MS = 30_000;

function spawnServer(file: string, token: string) {
	return Bun.spawn([serverBinary(), "--file", file, "--studio", "--http", "127.0.0.1:0", "--exit-on-stdin-close"], {
		env: serverEnvironment(process.env, token),
		// The server's way to know the app is gone, held by the FileServer for its whole life.
		stdin: "pipe",
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

	let announce!: (where: { origin: string } | { refused: string }) => void;
	const listening = new Promise<{ origin: string } | { refused: string }>((resolve) => (announce = resolve));
	// Drain stderr for the life of the process: a pipe nobody reads fills up
	// and blocks the server's log writes. Its promise ends with the stream.
	const reading = (async () => {
		const decoder = new TextDecoder();
		let pending = "";
		for await (const chunk of proc.stderr as unknown as AsyncIterable<Uint8Array>) {
			pending += decoder.decode(chunk, { stream: true });
			let i: number;
			while ((i = pending.indexOf("\n")) >= 0) {
				const line = pending.slice(0, i);
				pending = pending.slice(i + 1);
				note(line);
				// Only the server's own line, only on the loopback address that was asked for (listening.ts).
				const where = parseListening(line);
				if (where) announce(where);
			}
		}
		if (pending) note(pending);
	})();

	const exited = proc.exited.then((code) => code as number | null);
	const timeout = new Promise<"timeout">((r) => setTimeout(() => r("timeout"), START_TIMEOUT_MS));
	const first = await Promise.race([listening, exited.then(() => "exited" as const), timeout]);
	if (typeof first === "object" && "refused" in first) {
		// Not the server this app started, or one that went where it was not asked to: no token goes there.
		proc.kill("SIGKILL");
		await exited;
		throw new StartFailed(`fenec-server says it listens on ${first.refused}, not on 127.0.0.1 as it was asked to`, null);
	}
	if (first === "exited" || first === "timeout") {
		if (first === "timeout") proc.kill("SIGKILL");
		const code = await exited;
		// The last line it wrote is the reason: the stream's end says it is all here.
		await reading;
		const text = tail.join("\n");
		if (text.includes("it is open already")) throw new AlreadyOpen(text);
		throw new StartFailed(text || `fenec-server ${first === "timeout" ? "did not start in time" : "exited"}`, first === "timeout" ? null : code);
	}

	let stopping: Promise<number | null> | null = null;
	return {
		file,
		stdin: proc.stdin,
		origin: first.origin,
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
				return proc.exitCode;
			})();
			return stopping;
		},
	};
}
