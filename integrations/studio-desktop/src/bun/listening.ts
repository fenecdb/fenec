// The one line of a server's stderr the app acts on: where it listens.
//
//   fenec-http 0.1.12 listening on: http://127.0.0.1:52341  [token]
//
// (crates/fenec-http/src/lib.rs). Anchored to the whole line, not found
// anywhere in it: stderr also carries what a database says about its own data
// (a collection's name, a log of a statement), and a line that merely
// *contains* `listening on: http://...` must not name the server's address,
// which is where the token is then sent. And the address must be loopback: the
// app asked for 127.0.0.1, and a server that listens elsewhere is not the one
// it started. Pure, so `node --test` holds it (test/listening.test.ts).

const LINE = /^fenec-http \S+ listening on: (http:\/\/\S+)(?:\s|$)/;
const LOOPBACK = /^http:\/\/127\.0\.0\.1:\d{1,5}$/;

export type Listening = { origin: string } | { refused: string } | null;

/** `null`: not that line. `refused`: that line, naming an address that is not the loopback one asked for. */
export function parseListening(line: string): Listening {
	const m = LINE.exec(line);
	if (!m) return null;
	return LOOPBACK.test(m[1]!) ? { origin: m[1]! } : { refused: m[1]! };
}
