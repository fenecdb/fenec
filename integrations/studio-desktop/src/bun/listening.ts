// The lines of a server's stderr the app acts on: where it listens, and where
// its metrics are.
//
//   fenec-http 0.1.12 listening on: http://127.0.0.1:52341  [token]
//   metrics on: http://127.0.0.1:52340/_metrics  [token]
//
// (crates/fenec-http/src/lib.rs). Anchored to the whole line, not found
// anywhere in it: stderr also carries what a database says about its own data
// (a collection's name, a log of a statement), and a line that merely
// *contains* `listening on: http://...` must not name the server's address,
// which is where the token is then sent. And the address must be the one the
// app asked for: 127.0.0.1 for a window's server and a database served to this
// Mac, 0.0.0.0 for one served to the network, and the port that was asked for
// unless it was 0. A server that listens elsewhere is not the one it started.
// Pure, so `node --test` holds it (test/listening.test.ts).

const LINE = /^fenec-http \S+ listening on: (http:\/\/\S+)(?:\s|$)/;
const METRICS = /^metrics on: (http:\/\/127\.0\.0\.1:\d{1,5})\/_metrics(?:\s|$)/;

export type Listening = { origin: string; port: number } | { refused: string } | null;

/** The hosts the app starts a server on. */
export type Host = "127.0.0.1" | "0.0.0.0";

/**
 * `null`: not that line. `refused`: that line, naming an address that is not
 * on `host`, or not on `port` when one other than 0 was asked for.
 */
export function parseListening(line: string, host: Host = "127.0.0.1", port = 0): Listening {
	const m = LINE.exec(line);
	if (!m) return null;
	const where = m[1]!;
	const at = new RegExp(`^http://${host.replace(/\./g, "\\.")}:(\\d{1,5})$`).exec(where);
	const got = at ? Number(at[1]) : NaN;
	if (!at || got < 1 || got > 65535 || (port !== 0 && got !== port)) return { refused: where };
	return { origin: where, port: got };
}

/** The metrics endpoint's origin, from its own line; `null` for any other line. The app asks for it on loopback alone. */
export function parseMetrics(line: string): string | null {
	return METRICS.exec(line)?.[1] ?? null;
}
