// How a fenec-server is started and watched, over a `spawn` handed in: Bun's
// in the app (server.ts), Node's in `node --test`, which drives the real
// binary through this same code (test/process.test.ts).
//
//   fenec-server --file <path> --http <host>:<port> --sync <s> ... --exit-on-stdin-close
//
// (serving.ts writes the arguments) with its tokens in its environment, never
// in argv: `ps` shows argv to every user. Found by the `listening on:
// http://<address>` line on stderr, which is how every harness of the
// repository finds a server that listens on port 0.
//
// Its stdin is a pipe this process holds open for as long as the server runs
// (`FileServer.stdin`). A process that is killed sends its children no signal,
// but the system closes every descriptor it held, so the server sees the pipe
// end however the app ended -- `kill -9` too -- and shuts down as on SIGTERM:
// sync, checkpoint, `<file>.lock` let go, exit 0. That is why no server is left
// holding a file with nobody to answer. Stopped, it is sent SIGTERM, the same
// shutdown, and SIGKILL only if it has not ended 30 s later.
//
// Two things about it are not obvious. The pipe's write end must stay in this
// process alone: a child that inherited it (a descriptor not marked
// close-on-exec) would hold the server's stdin open after this process has
// gone, and `checks/orphan.mjs` kills the app with two servers up to see both
// end. And stderr and stdout are read for the server's whole life, since a pipe
// nobody reads fills, and the server's next log line waits for room; what they
// carry is kept in a bounded ring, the server's output the Servers list shows.

import { connect } from "node:net";
import { serverEnvironment } from "./env.ts";
import { parseListening, parseMetrics } from "./listening.ts";
import { serverArgs, type Plan } from "./serving.ts";

/** The file is open in another process: the server refused its `<file>.lock`. */
export class AlreadyOpen extends Error {}
/** The port asked for is taken by another program. */
export class PortTaken extends Error {
	port: number;
	constructor(message: string, port: number) {
		super(message);
		this.port = port;
	}
}
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

/** A started process, as both runtimes can give it. */
export type Child = {
	pid: number;
	/** The write end of its stdin, held for its life. */
	stdin: unknown;
	stdout: AsyncIterable<Uint8Array> | null;
	stderr: AsyncIterable<Uint8Array>;
	exited: Promise<number | null>;
	readonly exitCode: number | null;
	kill(signal: "SIGTERM" | "SIGKILL"): void;
};
export type Spawn = (argv: string[], env: Record<string, string>) => Child;

export type FileServer = {
	file: string;
	plan: Plan;
	/** The pipe on its stdin, which the server watches for its end; kept so it is not let go while the server runs. */
	stdin: unknown;
	/** Where it said it listens: `http://127.0.0.1:52341`, or `http://0.0.0.0:8080` for one served to the network. */
	origin: string;
	/** Where the app and its windows reach it: always loopback. */
	local: string;
	port: number;
	/** Its metrics endpoint's origin, when it has one of its own. */
	metrics: string | null;
	token: string;
	/** What `fenec backup` asks for its image with; null under `--sync off`. */
	replicationToken: string | null;
	pid: number;
	startedAt: number;
	/** SIGTERM, then SIGKILL after `graceMs`. Resolves once the process is gone. */
	stop(graceMs?: number): Promise<number | null>;
	/** Resolves when the process exits, for whatever reason. */
	exited: Promise<number | null>;
	/** The last lines it wrote, stderr and stdout, joined. */
	log(): string;
	/** The same, a line an entry. */
	lines(): string[];
	/** Called with each line as it comes; returns what stops it. */
	onLine(f: (line: string) => void): () => void;
};

/** 32 random bytes, base64url: a token for one start, or one kept for a served database. */
export function randomToken(): string {
	const bytes = crypto.getRandomValues(new Uint8Array(32));
	return Buffer.from(bytes).toString("base64url");
}

export const START_TIMEOUT_MS = 30_000;
/** The output kept: the last this many lines, each cut at LINE_CHARS. At most about 1 MB a server. */
export const LOG_LINES = 1000;
export const LINE_CHARS = 1000;

/** Reads `stream` a line at a time until it ends. */
async function eachLine(stream: AsyncIterable<Uint8Array>, f: (line: string) => void) {
	const decoder = new TextDecoder();
	let pending = "";
	for await (const chunk of stream) {
		pending += decoder.decode(chunk, { stream: true });
		let i: number;
		while ((i = pending.indexOf("\n")) >= 0) {
			f(pending.slice(0, i));
			pending = pending.slice(i + 1);
		}
		// A line that never ends is cut, not held whole.
		if (pending.length > LINE_CHARS * 4) {
			f(pending);
			pending = "";
		}
	}
	if (pending) f(pending);
}

/**
 * Does anything accept a connection on loopback at `port`? A bind does not
 * say: the server binds with SO_REUSEADDR, under which macOS lets
 * `0.0.0.0:8080` and `127.0.0.1:8080` be bound by two programs at once, and a
 * connection to 127.0.0.1 goes to whichever bound it by name. The app and its
 * windows reach every server it starts on 127.0.0.1, with the token: so a port
 * something answers on there is another program's, whatever a bind would say.
 */
export function portAnswers(port: number, timeoutMs = 1000): Promise<boolean> {
	return new Promise((resolve) => {
		const socket = connect({ host: "127.0.0.1", port });
		const done = (answered: boolean) => {
			socket.destroy();
			resolve(answered);
		};
		socket.setTimeout(timeoutMs, () => done(false));
		socket.once("connect", () => done(true));
		socket.once("error", () => done(false));
	});
}

export async function startServer(
	spawn: Spawn,
	binary: string,
	plan: Plan,
	token: string,
	env: Record<string, string | undefined> = process.env,
	startTimeoutMs = START_TIMEOUT_MS,
): Promise<FileServer> {
	// A port the system picks is one nothing holds; a port asked for may be another program's.
	if (plan.port !== 0 && (await portAnswers(plan.port))) {
		throw new PortTaken(`something already answers on 127.0.0.1:${plan.port}, so the app would reach it rather than this server`, plan.port);
	}
	const replicationToken = plan.replication ? randomToken() : null;
	const proc = spawn([binary, ...serverArgs(plan)], serverEnvironment(env, token, replicationToken));
	const tail: string[] = [];
	const listeners = new Set<(line: string) => void>();
	let metrics: string | null = null;

	let announce!: (where: { origin: string; port: number } | { refused: string }) => void;
	const listening = new Promise<{ origin: string; port: number } | { refused: string }>((resolve) => (announce = resolve));
	const note = (raw: string) => {
		const line = raw.length > LINE_CHARS ? `${raw.slice(0, LINE_CHARS)}…` : raw;
		tail.push(line);
		if (tail.length > LOG_LINES) tail.shift();
		metrics ??= parseMetrics(line);
		// Only the server's own line, only on the address that was asked for (listening.ts).
		const where = parseListening(line, plan.host, plan.port);
		if (where) announce(where);
		listeners.forEach((f) => f(line));
	};
	// Drain both for the life of the process: a pipe nobody reads fills up
	// and blocks the server's log writes. Each promise ends with its stream.
	const reading = Promise.all([eachLine(proc.stderr, note), proc.stdout ? eachLine(proc.stdout, note) : Promise.resolve()]);

	const exited = proc.exited;
	let timer: ReturnType<typeof setTimeout> | undefined;
	const timeout = new Promise<"timeout">((r) => (timer = setTimeout(() => r("timeout"), startTimeoutMs)));
	const first = await Promise.race([listening, exited.then(() => "exited" as const), timeout]);
	clearTimeout(timer);
	if (typeof first === "object" && "refused" in first) {
		// Not the server this app started, or one that went where it was not asked to: no token goes there.
		proc.kill("SIGKILL");
		await exited;
		throw new StartFailed(`fenec-server says it listens on ${first.refused}, not on ${plan.host}${plan.port ? `:${plan.port}` : ""} as it was asked to`, null);
	}
	if (first === "exited" || first === "timeout") {
		if (first === "timeout") proc.kill("SIGKILL");
		const code = await exited;
		// The last line it wrote is the reason: the streams' end says it is all here.
		await reading;
		const text = tail.join("\n");
		if (text.includes("it is open already")) throw new AlreadyOpen(text);
		if (/address already in use/i.test(text)) throw new PortTaken(text, plan.port);
		throw new StartFailed(text || `fenec-server ${first === "timeout" ? "did not start in time" : "exited"}`, first === "timeout" ? null : code);
	}

	let stopping: Promise<number | null> | null = null;
	return {
		file: plan.file,
		plan,
		stdin: proc.stdin,
		origin: first.origin,
		local: `http://127.0.0.1:${first.port}`,
		port: first.port,
		get metrics() {
			return metrics;
		},
		token,
		replicationToken,
		pid: proc.pid,
		startedAt: Date.now(),
		exited,
		log: () => tail.join("\n"),
		lines: () => [...tail],
		onLine(f) {
			listeners.add(f);
			return () => void listeners.delete(f);
		},
		stop(graceMs = 30_000) {
			stopping ??= (async () => {
				if (proc.exitCode === null) proc.kill("SIGTERM");
				let timer: ReturnType<typeof setTimeout> | undefined;
				const done = await Promise.race([exited, new Promise<"late">((r) => (timer = setTimeout(() => r("late"), graceMs)))]);
				clearTimeout(timer);
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
