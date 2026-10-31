// One instance of the app at a time. Two would each keep state.json and the
// Keychain items as if they were alone: one's Remove would delete the token
// under a server the other runs, and each would write the other's lists over.
// macOS opens a running app rather than a second one from Finder, the Dock and
// `open`, but not from `open -n` or the binary run by hand; Electrobun 2.0.4
// has no single-instance switch. So the first instance listens on a socket of
// its own in the user's temporary folder (0700, the user's alone), and a later
// one that reaches it asks it to show itself and quits. A socket left by an
// instance that ended answers nothing, and is taken over. Pure but for the
// socket, so `node --test` holds it (test/instance.test.ts).

import { unlinkSync } from "node:fs";
import { connect, createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

/** The socket of the app `identifier` (the test variant has its own), per user. */
export const instancePath = (identifier: string, folder = tmpdir()) => join(folder, `${identifier}.instance`);

/** Does an instance answer at `path`? If one does, it is asked to show itself. */
function askOther(path: string): Promise<boolean> {
	return new Promise((resolve) => {
		const socket = connect(path);
		socket.once("connect", () => socket.end("show\n", () => resolve(true)));
		socket.once("error", () => resolve(false));
	});
}

type Listened = "listening" | "taken" | "failed";

/** Listens at `path` for later instances; "taken": something is there already. */
function listen(path: string, onAsk: () => void): Promise<Listened> {
	return new Promise((resolve) => {
		const server = createServer((c) => {
			c.on("error", () => {});
			c.once("data", () => onAsk());
			c.resume();
		});
		server.once("error", (e: NodeJS.ErrnoException) => resolve(e.code === "EADDRINUSE" ? "taken" : "failed"));
		server.listen(path, () => {
			// The socket keeps no event loop alive: the app's windows do.
			server.unref();
			resolve("listening");
		});
	});
}

/**
 * `true`: this is the one instance, and `onAsk` is called whenever another
 * starts. `false`: another runs, and has been asked to show itself; this one
 * is to quit before it opens anything.
 *
 * The bind is the claim, made first, so two instances started together cannot
 * both have it: the second's bind fails, and it asks the first. A socket an
 * instance that ended left answers nothing and is taken over; only two
 * instances started together beside such a socket could both take it, in the
 * moment between one's look and its unlink. A folder the socket cannot be made
 * in (a `TMPDIR` that is not the user's own) is no reason not to start: the app
 * then runs without the guard.
 */
export async function claimInstance(path: string, onAsk: () => void): Promise<boolean> {
	for (let attempt = 0; attempt < 3; attempt++) {
		const listened = await listen(path, onAsk);
		if (listened !== "taken") return true;
		if (await askOther(path)) return false;
		// Nothing answered: what is there was left by an instance that ended.
		try {
			unlinkSync(path);
		} catch {
			// Taken away meanwhile by another instance starting.
		}
	}
	// Three binds lost to sockets that answer nothing: run unguarded rather than not at all.
	return true;
}
