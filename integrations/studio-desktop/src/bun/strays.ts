// Servers an earlier run of the app left behind.
//
// A killed or crashed app cannot stop its children (macOS has no
// parent-death signal), and an orphaned server keeps `<file>.lock`: the next
// open of that file is refused. So every server started is noted in
// userData/servers.json, and the next launch stops the ones still running
// that are still the fenec-server it started on that file: SIGTERM, a wait for
// each to be gone, SIGKILL after a grace period, and whatever survives even
// that stays noted, so a later launch tries again.
//
// This goes away with `fenec-server --exit-on-stdin-close`: a server whose
// app is gone sees its stdin close and shuts down by itself (server.ts).
//
// Pure over `node:fs` and `ps`, with the process calls injectable, so
// `node --test` holds it with real processes (test/strays.test.ts).

import { spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

export type Entry = { pid: number; file: string };

export type Io = {
	/** Is `pid` still the server this app started on `file`? */
	isOurs(pid: number, file: string): boolean;
	alive(pid: number): boolean;
	signal(pid: number, sig: "SIGTERM" | "SIGKILL"): void;
};

/**
 * A pid is reused, after a reboot above all, and the registry outlives a
 * reboot: stopping "whatever has that pid" could stop a fenec-server the user
 * started themselves. Its arguments say whose it is: `--file <this file>` and
 * `--studio`, which a user's own server of the same file would hold only by
 * chance.
 */
function isOurs(pid: number, file: string): boolean {
	const ps = spawnSync("ps", ["-p", String(pid), "-o", "args="], { encoding: "utf8" });
	const args = (ps.stdout ?? "").trim();
	return /(^|\/)fenec-server /.test(args) && args.includes(` --file ${file} `) && args.includes(" --studio ");
}

export const realIo: Io = {
	isOurs,
	alive(pid) {
		try {
			process.kill(pid, 0);
			return true;
		} catch (e) {
			// EPERM: there, and not ours to signal.
			return (e as NodeJS.ErrnoException).code === "EPERM";
		}
	},
	signal(pid, sig) {
		try {
			process.kill(pid, sig);
		} catch {
			// gone between the look and the signal
		}
	},
};

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

export class Strays {
	private readonly path: string;
	private readonly io: Io;

	constructor(path: string, io: Io = realIo) {
		this.path = path;
		this.io = io;
	}

	private read(): Entry[] {
		try {
			const list = JSON.parse(readFileSync(this.path, "utf8"));
			return Array.isArray(list) ? list : [];
		} catch {
			return [];
		}
	}

	private write(list: Entry[]) {
		mkdirSync(dirname(this.path), { recursive: true });
		writeFileSync(this.path, JSON.stringify(list));
	}

	remember(pid: number, file: string) {
		this.write([...this.read().filter((e) => e.pid !== pid), { pid, file }]);
	}

	forget(pid: number) {
		this.write(this.read().filter((e) => e.pid !== pid));
	}

	/** Those of `list` still running, after waiting up to `ms` for them to end. */
	private async gone(list: Entry[], ms: number, pollMs: number): Promise<Entry[]> {
		const until = Date.now() + ms;
		let alive = list.filter((e) => this.io.alive(e.pid));
		while (alive.length && Date.now() < until) {
			await sleep(pollMs);
			alive = alive.filter((e) => this.io.alive(e.pid));
		}
		return alive;
	}

	/**
	 * Stops every server a previous run left behind. Resolves once each is gone
	 * (the event the caller waits for: the file's lock is free), or its time is
	 * up. Returns the files whose servers ended, and the entries that did not.
	 */
	async reap({ graceMs = 10_000, killMs = 3_000, pollMs = 50 } = {}): Promise<{ reaped: string[]; left: Entry[] }> {
		const ours = this.read().filter((e) => this.io.isOurs(e.pid, e.file));
		for (const e of ours) this.io.signal(e.pid, "SIGTERM");
		// Together, not one after another: each is writing its checkpoint.
		let alive = await this.gone(ours, graceMs, pollMs);
		for (const e of alive) if (this.io.isOurs(e.pid, e.file)) this.io.signal(e.pid, "SIGKILL");
		alive = await this.gone(alive, killMs, pollMs);
		// What is not ours (a reused pid, a server that ended) is dropped; what
		// survived even SIGKILL stays noted for the next launch.
		this.write(alive);
		return { reaped: ours.filter((e) => !alive.includes(e)).map((e) => e.file), left: alive };
	}
}
