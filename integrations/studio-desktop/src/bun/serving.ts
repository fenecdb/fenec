// What a server the app starts is started with, and how it is told to the
// user: the arguments, the facts a served database's card shows, the command
// the serve sheet prints, and the snippets an app connects with. Pure, so
// `node --test` holds it (test/serving.test.ts); the main process, the serve
// sheet and the Servers list all read it, so what the sheet shows is what runs.
//
// Two kinds of server, one way to start them (server.ts):
//   - a window's: a file opened in the studio, 127.0.0.1 on a port the system
//     picks, a token made for that start, stopped with its window;
//   - a served database's: the port and the reach the user picked, its token
//     kept in the Keychain so the apps that hold it keep working, running
//     until it is stopped or the app quits.
// Both take their secrets in their environment (env.ts) and watch their stdin
// for the app's end (`--exit-on-stdin-close`).

import type { Host } from "./listening.ts";

/** When a write is on disk: `--sync always`, `--sync 250`, `--sync off`. */
export type Sync = "always" | "250" | "off";
/** Who can reach a served database: this Mac (127.0.0.1) or the network (0.0.0.0). */
export type Reach = "mac" | "lan";

export type ServeSettings = {
	/** `null`: any free port, which the server names on its `listening on:` line. */
	port: number | null;
	reach: Reach;
	/** `--studio`. */
	studio: boolean;
	/** The server's own compaction, on by default (`--auto-compact off` turns it off). */
	compact: boolean;
	/** `--mcp`. */
	mcp: boolean;
	/** `--metrics 127.0.0.1:0`, on a port of its own. */
	metrics: boolean;
};

export const DEFAULT_SYNC: Sync = "250";
export const DEFAULT_SETTINGS: ServeSettings = { port: null, reach: "mac", studio: true, compact: true, mcp: false, metrics: false };

export const isSync = (v: unknown): v is Sync => v === "always" || v === "250" || v === "off";

/** Settings read back from state.json, whatever was there: what is not one is the default. */
export function settingsFrom(v: unknown): ServeSettings {
	const o = (v && typeof v === "object" ? v : {}) as Record<string, unknown>;
	const port = typeof o["port"] === "number" && Number.isInteger(o["port"]) && o["port"] >= 1 && o["port"] <= 65535 ? o["port"] : null;
	const flag = (k: keyof ServeSettings) => (typeof o[k] === "boolean" ? (o[k] as boolean) : (DEFAULT_SETTINGS[k] as boolean));
	return { port, reach: o["reach"] === "lan" ? "lan" : "mac", studio: flag("studio"), compact: flag("compact"), mcp: flag("mcp"), metrics: flag("metrics") };
}

export const hostOf = (reach: Reach): Host => (reach === "lan" ? "0.0.0.0" : "127.0.0.1");

/** A port as typed in the serve sheet: empty is any free port. */
export function parsePort(typed: string): { ok: true; port: number | null } | { ok: false; message: string } {
	const t = typed.trim();
	if (t === "") return { ok: true, port: null };
	if (!/^\d{1,5}$/.test(t) || Number(t) < 1 || Number(t) > 65535) return { ok: false, message: "A port is a number from 1 to 65535, or empty for any free port." };
	return { ok: true, port: Number(t) };
}

/** Everything a server is started with but its secrets. */
export type Plan = {
	file: string;
	host: Host;
	/** 0: any free port. */
	port: number;
	sync: Sync;
	studio: boolean;
	compact: boolean;
	mcp: boolean;
	metrics: boolean;
	/**
	 * A replication token in its environment, which `fenec backup` asks for its
	 * image with. Never with `--sync off`, which the server refuses it with: a
	 * replica, and a backup, are sent only what is on disk.
	 */
	replication: boolean;
};

/** A window's server: this Mac, any port, the studio, and the file's own `sync`. */
export function windowPlan(file: string, sync: Sync): Plan {
	return { file, host: "127.0.0.1", port: 0, sync, studio: true, compact: true, mcp: false, metrics: false, replication: sync !== "off" };
}

/** A server started only to take a backup or compact a file that nothing serves: no studio, nothing else. */
export function taskPlan(file: string): Plan {
	// It takes no write of its own, so `--sync off` would gain nothing and lose the backup.
	return { file, host: "127.0.0.1", port: 0, sync: "250", studio: false, compact: true, mcp: false, metrics: false, replication: true };
}

/** A served database's server. */
export function servePlan(file: string, sync: Sync, s: ServeSettings): Plan {
	return { file, host: hostOf(s.reach), port: s.port ?? 0, sync, studio: s.studio, compact: s.compact, mcp: s.mcp, metrics: s.metrics, replication: sync !== "off" };
}

/**
 * The arguments, after the binary. No secret is among them: the tokens are in
 * the environment (env.ts), since `ps` shows every user a process's arguments.
 */
export function serverArgs(p: Plan): string[] {
	const a = ["--file", p.file, "--http", `${p.host}:${p.port}`, "--sync", p.sync];
	if (p.studio) a.push("--studio");
	// 0.5 is the server's default: only "off" needs saying.
	if (!p.compact) a.push("--auto-compact", "off");
	if (p.mcp) a.push("--mcp");
	// Metrics stay on this Mac whoever may reach the data: a Prometheus here scrapes them.
	if (p.metrics) a.push("--metrics", "127.0.0.1:0");
	// The writes a server keeps for a replica that falls behind: a backup takes
	// an image and needs none of them, so 1 MiB rather than the default 64.
	if (p.replication) a.push("--replication-buffer", "1");
	a.push("--exit-on-stdin-close");
	return a;
}

/** A path as a shell would need it, with the home folder as `~`. For the command shown, never run. */
function shown(path: string, home: string): string {
	const p = home && (path === home || path.startsWith(home + "/")) ? "~" + path.slice(home.length) : path;
	return /^[\w@%+=:,./~-]+$/.test(p) ? p : `'${p.replace(/'/g, `'\\''`)}'`;
}

/** What the serve sheet prints under "What the app runs": the command, its secrets as dots. */
export function commandShown(p: Plan, home = ""): string {
	const args = serverArgs(p).map((a, i, all) => (all[i - 1] === "--file" ? shown(a, home) : a));
	// One line a flag and its value.
	const lines: string[] = [];
	for (const a of args) {
		if (a.startsWith("--") || lines.length === 0) lines.push(a);
		else lines[lines.length - 1] += ` ${a}`;
	}
	const secrets = `FENEC_HTTP_TOKEN=••••${p.replication ? " FENEC_REPLICATION_TOKEN=••••" : ""}`;
	return `${secrets} fenec-server ${lines.join(" \\\n  ")}`;
}

const SYNC_WORDS: Record<Sync, string> = { always: "every write synced", "250": "synced every 250 ms", off: "synced when the system writes" };

/** What a served database's card says it runs with: "This Mac only · token required · synced every 250 ms · studio on". */
export function factsOf(s: ServeSettings, sync: Sync): string {
	return [
		s.reach === "lan" ? "your network" : "This Mac only",
		"token required",
		SYNC_WORDS[sync],
		s.studio ? "studio on" : "no studio",
		s.compact ? "compacts by itself" : "compacts when asked",
		...(s.mcp ? ["MCP on"] : []),
		...(s.metrics ? ["metrics on a port of their own"] : []),
	]
		.join(" · ")
		.replace(/^./, (c) => c.toUpperCase());
}

/** The note under each choice of the new-database window's "When a write is safe on disk". */
export const SYNC_NOTES: Record<Sync, string> = {
	always: "Each write is answered only after it is on disk. The safest, and the slowest on a laptop.",
	"250": "Writes are synced to disk every 250 ms. A crash can lose the last quarter second; nothing is ever torn.",
	off: "The operating system decides when to write. Fine for scratch data you can make again; it cannot be backed up while it runs.",
};

export type Snippets = { curl: string; js: string; py: string; env: string };

/**
 * How an app connects, for the Servers list's "Connect from your app". The
 * token stays out: each reads `FENEC_TOKEN` from its environment, and the
 * token is copied from the serve sheet.
 */
export function snippets(url: string, collection = "orders"): Snippets {
	const q = `get ${collection} limit 5`;
	return {
		curl: `curl -s ${url}/query \\\n  -H "Authorization: Bearer $FENEC_TOKEN" \\\n  -d '{"query": "${q}"}'`,
		js: `import { connect } from '@fenecdb/web/client';\n\nconst db = connect('${url}', { token: process.env.FENEC_TOKEN });\nconst rows = await db.run('${q}');`,
		py: `import os\nfrom fenecdb import Client\n\ndb = Client("${url}", token=os.environ["FENEC_TOKEN"])\nrows = db.query("${q}")`,
		env: `FENEC_URL=${url}\nFENEC_TOKEN=<copy it from Settings>`,
	};
}
