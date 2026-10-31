// What the app remembers between runs, in userData/state.json: the recent
// files, the saved connections (name, URL, tenant), and for each database the
// app has made, served or backed up, how: its sync policy, the settings it was
// last served with, where its backups go. No secret is here: a connection's
// token is in the keychain under `connection:<id>`, a served database's under
// `serve:<id>` (secrets.ts, token.ts), and a backup's key is a file the user
// keeps, of which only the path is here.

import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { Utils } from "electrobun/main";
import { DEFAULT_SYNC, isSync, settingsFrom, type ServeSettings, type Sync } from "./serving.ts";

export type StoredConnection = { id: string; name: string; url: string; tenant: string | null };

export type StoredDatabase = {
	/** Its real path. */
	path: string;
	/** Names its served token's Keychain item. */
	id: string;
	/** What every server the app starts on it is started with. */
	sync: Sync;
	/** Set once it was served: the Servers list shows it, stopped or running, and Start uses these. */
	serve?: ServeSettings;
	/** When its served server last stopped, on which port, and why. */
	lastRan?: { at: number; port: number | null; why: string };
	/** What its server said last time one ran: shown while none does. */
	seen?: { collections: number; rows: number; fileBytes: number; reclaimable: number; at: number; first?: string };
	/** Where its backups go, sealed with the key file named, and the last one taken. */
	archive?: { dir: string; keyFile: string | null; lastAt?: number; lastChange?: number };
};

type State = { recents: { path: string; openedAt: number }[]; connections: StoredConnection[]; databases: StoredDatabase[] };

const file = () => join(Utils.paths.userData, "state.json");
const MAX_RECENTS = 12;

/** One read back as it was written, whatever is there: what is not one is dropped or made the default. */
function database(v: any): StoredDatabase | null {
	if (!v || typeof v.path !== "string" || typeof v.id !== "string") return null;
	const d: StoredDatabase = { path: v.path, id: v.id, sync: isSync(v.sync) ? v.sync : DEFAULT_SYNC };
	if (v.serve) d.serve = settingsFrom(v.serve);
	if (v.lastRan && typeof v.lastRan.at === "number") d.lastRan = { at: v.lastRan.at, port: typeof v.lastRan.port === "number" ? v.lastRan.port : null, why: String(v.lastRan.why ?? "") };
	if (v.seen && typeof v.seen.at === "number") d.seen = v.seen;
	if (v.archive && typeof v.archive.dir === "string") d.archive = { ...v.archive, keyFile: typeof v.archive.keyFile === "string" ? v.archive.keyFile : null };
	return d;
}

function read(): State {
	try {
		const s = JSON.parse(readFileSync(file(), "utf8"));
		return {
			recents: s.recents ?? [],
			connections: s.connections ?? [],
			databases: (Array.isArray(s.databases) ? s.databases : []).map(database).filter((d: StoredDatabase | null): d is StoredDatabase => d !== null),
		};
	} catch {
		return { recents: [], connections: [], databases: [] };
	}
}

function write(s: State) {
	mkdirSync(dirname(file()), { recursive: true });
	const tmp = `${file()}.tmp`;
	writeFileSync(tmp, JSON.stringify(s, null, 2));
	renameSync(tmp, file());
}

export const store = {
	recents: () => read().recents,
	touch(path: string) {
		const s = read();
		s.recents = [{ path, openedAt: Date.now() }, ...s.recents.filter((r) => r.path !== path)].slice(0, MAX_RECENTS);
		write(s);
	},
	/** Out of the list: its recent entry and what the app kept for it. Its served token is the caller's to delete. */
	forget(path: string) {
		const s = read();
		s.recents = s.recents.filter((r) => r.path !== path);
		s.databases = s.databases.filter((d) => d.path !== path);
		write(s);
	},
	databases: () => read().databases,
	database: (path: string) => read().databases.find((d) => d.path === path) ?? null,
	/** The entry for `path`, made with the defaults if there is none, after `change` was applied to it. */
	updateDatabase(path: string, change: (d: StoredDatabase) => void = () => {}): StoredDatabase {
		const s = read();
		let d = s.databases.find((x) => x.path === path);
		if (!d) {
			d = { path, id: crypto.randomUUID(), sync: DEFAULT_SYNC };
			s.databases.push(d);
		}
		change(d);
		write(s);
		return d;
	},
	connections: () => read().connections,
	saveConnection(c: StoredConnection) {
		const s = read();
		// A change keeps the connection where the list had it.
		s.connections = s.connections.some((x) => x.id === c.id) ? s.connections.map((x) => (x.id === c.id ? c : x)) : [...s.connections, c];
		write(s);
	},
	removeConnection(id: string) {
		const s = read();
		s.connections = s.connections.filter((x) => x.id !== id);
		write(s);
	},
};
