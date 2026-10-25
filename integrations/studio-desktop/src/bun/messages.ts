// Every failure the app tells the user about, in one place: what happened,
// what to do about it, and what the server said (for a report). Each becomes
// a notice window (windows.ts), never a native alert: a native message box is
// a synchronous call that stops the main process's JavaScript -- every
// server's supervision and the quit handler with it -- until someone clicks.
// Pure, so `node --test` can hold the wording to its cases (test/messages.test.ts).

import type { Notice } from "../shared/rpc.ts";
import type { Probe } from "./remote.ts";

const tail = (text: string, lines = 8) => text.trim().split("\n").slice(-lines).join("\n");

/** The server found the file's lock taken: another program has the file open. */
export function heldNotice(name: string, serverSaid: string): Notice {
	return {
		title: "This database is open in another program",
		message: `${name} is open in another program: another fenec studio, a fenec-server, or an app that uses the file.`,
		hint: "Close it there first. Two programs over one file would corrupt it.",
		detail: tail(serverSaid),
		actions: [{ id: "retry", label: "Try again" }],
	};
}

/** The file is open in a window of this app already (a second open only shows it). */
export function alreadyHereMessage(name: string): string {
	return `${name} is already open in a window of this app; showing it.`;
}

export function missingFile(path: string): Notice {
	return {
		title: "That file is gone",
		message: `${path} does not exist any more.`,
		hint: "It was moved or deleted. Choose Open to find it again.",
	};
}

export function notAFile(path: string): Notice {
	return { title: "That is not a database file", message: `${path} is not a file.` };
}

/**
 * An empty file is made a database by the first server that opens it, which is
 * what New Database does on purpose and Open must not do to a file someone
 * picked by mistake.
 */
export function emptyFile(name: string): Notice {
	return {
		title: "That file is empty",
		message: `${name} has nothing in it, so it is not a database.`,
		hint: "Nothing was changed in it. To make a new database, choose File, New Database.",
	};
}

/** A file whose first bytes are not a fenecdb signature, found before any server was started on it. */
export function notADatabase(name: string): Notice {
	return {
		title: "That is not a fenecdb database",
		message: `${name} does not start with a fenecdb signature, so it is some other kind of file.`,
		hint: "Nothing was changed in it and nothing was written beside it. Pick a .fenec file, or make a new database.",
	};
}

/** The server could not start, for a reason other than a lock. */
export function startFailedNotice(name: string, serverSaid: string): Notice {
	// A file picked by mistake: the server reads its signature first and
	// refuses before it writes anything.
	if (/invalid fenecdb signature/.test(serverSaid)) {
		return {
			title: "That is not a fenecdb database",
			message: `${name} does not start with a fenecdb signature, so it is some other kind of file.`,
			hint: "Nothing was changed in it. Pick a .fenec file, or make a new database.",
			detail: tail(serverSaid),
		};
	}
	return {
		title: "Could not open the database",
		message: `fenec-server could not open ${name}.`,
		hint: "The file may be damaged, from a newer version, or on a disk that cannot be read.",
		detail: tail(serverSaid),
		actions: [{ id: "retry", label: "Try again" }],
	};
}

/** The server a window was reading from exited under it. */
export function diedNotice(name: string, exit: number | null, serverSaid: string): Notice {
	return {
		title: "The database server stopped",
		message: `The fenec-server behind ${name} exited (${exit === null ? "killed by a signal" : `code ${exit}`}), so its window was closed.`,
		hint: "Nothing written before it stopped is lost. Open the file again to carry on.",
		detail: tail(serverSaid),
		actions: [{ id: "retry", label: "Open again" }],
	};
}

/** A saved server that a window cannot be opened on. */
export function remoteNotice(name: string, origin: string, probe: Exclude<Probe, { kind: "ok" }>): Notice {
	switch (probe.kind) {
		case "unreachable":
			return {
				title: "Could not reach the server",
				message: `${name} (${origin}): ${probe.reason}.`,
				hint: "Check that the server is running and that the address is right.",
				actions: [{ id: "edit", label: "Edit connection" }],
			};
		case "redirected":
			return {
				title: "The server sent us somewhere else",
				message: `${name} (${origin}) answers with a redirect (HTTP ${probe.status}) to ${probe.to}.`,
				hint: "The token is sent only to the address it was saved for. If that other address is the server, edit the connection to it.",
				actions: [{ id: "edit", label: "Edit connection" }],
			};
		case "no-studio":
			return {
				title: "This server has no studio",
				message: `${name} (${origin}) answers (HTTP ${probe.status}) but does not serve fenec studio.`,
				hint: "Start that fenec-server (or fenec-shard) with --studio, from a build that has it.",
			};
		case "token-refused":
			return {
				title: probe.hadToken ? "The server refused the saved token" : "This server asks for a token",
				message: probe.hadToken
					? `${name} (${origin}) did not accept the token saved for it: ${probe.detail}.`
					: `${name} (${origin}) needs a token and none is saved for it.`,
				hint: "Edit the connection and enter the server's current token.",
				actions: [{ id: "edit", label: "Edit connection" }],
			};
	}
}

export function gone(what: string): Notice {
	return { title: "That is gone", message: `${what} does not exist any more.` };
}

export function internalError(what: string, e: unknown): Notice {
	return { title: "Something went wrong", message: what, detail: String((e as Error)?.message ?? e) };
}
