// What the app remembers between runs, in userData/state.json: the recent
// files and the saved connections (name, URL, tenant). A connection's token is
// not here; it is in the keychain under `connection:<id>` (secrets.ts).

import { mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { Utils } from "electrobun/main";

export type StoredConnection = { id: string; name: string; url: string; tenant: string | null };
type State = { recents: { path: string; openedAt: number }[]; connections: StoredConnection[] };

const file = () => join(Utils.paths.userData, "state.json");
const MAX_RECENTS = 12;

function read(): State {
	try {
		const s = JSON.parse(readFileSync(file(), "utf8"));
		return { recents: s.recents ?? [], connections: s.connections ?? [] };
	} catch {
		return { recents: [], connections: [] };
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
	forgetRecent(path: string) {
		const s = read();
		s.recents = s.recents.filter((r) => r.path !== path);
		write(s);
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
