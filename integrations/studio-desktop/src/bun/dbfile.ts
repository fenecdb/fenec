// What a file the user picked is, before a server is started on it.
//
// `fenec-server --file` makes any file it is given into a database if it is
// empty (it writes the signature into it) and leaves `<file>.lock` beside any
// file it opens, so a stray pick (a text file, an empty one) would have a lock
// file, or a head, written next to it. The app looks first and starts a server
// only for a file that is already a database, or that it made itself (New
// database). Pure over `node:fs`, so `node --test` holds it (test/dbfile.test.ts).

import { closeSync, openSync, readSync, statSync } from "node:fs";

/**
 * The first seven bytes of a database file: `FENECDB`. The eighth is the
 * format's version (`MAGIC` in fenec-core's engine.rs, `FENECDB\x01`), which
 * the server judges, so a file from a newer format reaches it and is refused
 * there in its own words.
 */
export const SIGNATURE = "FENECDB";

export type Kind = "database" | "empty" | "other";

export function kindOfFile(path: string): Kind {
	if (statSync(path).size === 0) return "empty";
	const fd = openSync(path, "r");
	try {
		const head = Buffer.alloc(SIGNATURE.length);
		const n = readSync(fd, head, 0, head.length, 0);
		return n === head.length && head.toString("latin1") === SIGNATURE ? "database" : "other";
	} finally {
		closeSync(fd);
	}
}
