// Servers an earlier run of the app left behind.
//
// A killed or crashed app cannot stop its children (macOS has no
// parent-death signal), and an orphaned server keeps `<file>.lock`: the next
// open of that file is refused. So every server started is noted in
// userData/servers.json, and the next launch stops the ones still running
// that are still the fenec-server it started on that file.
//
// This goes away with `fenec-server --exit-on-stdin-close`: a server whose
// app is gone sees its stdin close and shuts down by itself (server.ts).

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { Utils } from "electrobun/main";

type Entry = { pid: number; file: string };

const registry = () => join(Utils.paths.userData, "servers.json");

function read(): Entry[] {
	try {
		const list = JSON.parse(readFileSync(registry(), "utf8"));
		return Array.isArray(list) ? list : [];
	} catch {
		return [];
	}
}

function write(list: Entry[]) {
	mkdirSync(Utils.paths.userData, { recursive: true });
	writeFileSync(registry(), JSON.stringify(list));
}

export function remember(pid: number, file: string) {
	write([...read().filter((e) => e.pid !== pid), { pid, file }]);
}

export function forget(pid: number) {
	write(read().filter((e) => e.pid !== pid));
}

/**
 * Is `pid` still the server this app started on `file`? A pid is reused, after
 * a reboot above all, and the registry outlives a reboot: stopping "whatever
 * has that pid" could stop a fenec-server the user started themselves. Its
 * arguments say whose it is: `--file <this file>` and `--studio`, which a
 * user's own server of the same file would hold only by chance.
 */
function isOurs(pid: number, file: string): boolean {
	const ps = Bun.spawnSync(["ps", "-p", String(pid), "-o", "args="]);
	const args = new TextDecoder().decode(ps.stdout).trim();
	return /(^|\/)fenec-server /.test(args) && args.includes(` --file ${file} `) && args.includes(" --studio ");
}

/** SIGTERM to every server a previous run left behind. Returns the files they held. */
export async function reapStrays(): Promise<string[]> {
	const reaped: string[] = [];
	for (const e of read()) {
		if (isOurs(e.pid, e.file)) {
			process.kill(e.pid, "SIGTERM");
			reaped.push(e.file);
		}
	}
	if (reaped.length) await Bun.sleep(1500);
	if (existsSync(registry())) write([]);
	return reaped;
}
