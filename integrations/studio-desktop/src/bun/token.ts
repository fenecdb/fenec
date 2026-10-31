// What the Keychain holds for a saved server: the token **and the origin it was
// saved for**, as one value. A token read back is a token for that address
// alone: if the connection's address has changed since (an edit that landed
// between the read and the use, one that left the old item behind, an item
// written by anything else), the answer is "no token", and nothing is sent to
// the new address. The Keychain and `state.json` are two stores written one
// after the other, so no order of writing them makes their pair atomic; the
// binding does not need one. Pure, so `node --test` holds it (test/token.test.ts).

export const keychainName = (connectionId: string) => `connection:${connectionId}`;

// A served database's token is kept the same way, under `serve:<id>` and
// sealed for `file:<real path>`: the address of a database the app serves
// changes with each start on any free port, its file does not. A file moved or
// renamed is another file, and is given a token of its own.

export const serveItemName = (databaseId: string) => `serve:${databaseId}`;
export const fileAddress = (path: string) => `file:${path}`;

/** The Keychain, as servedToken uses it (secrets.ts; a test hands in a map). */
export type KeyStore = {
	get(name: string): Promise<string | null>;
	set(name: string, value: string): Promise<void>;
	delete(name: string): Promise<void>;
};

/**
 * The token a served database's server is started with: the one kept for its
 * file, or, if there is none for it (never served, moved, `fresh` asked for by
 * "New token"), one made by `make` and kept before it is used, so a server never
 * runs with a token the Keychain does not hold.
 */
export async function servedToken(keys: KeyStore, id: string, path: string, make: () => string, fresh = false): Promise<string> {
	const kept = fresh ? "" : tokenFor(await keys.get(serveItemName(id)), fileAddress(path));
	if (kept) return kept;
	const token = make();
	await keys.set(serveItemName(id), sealToken(fileAddress(path), token));
	return token;
}

/** The Keychain's value for `token`, saved for `origin`. */
export function sealToken(origin: string, token: string): string {
	return JSON.stringify({ origin, token });
}

/** The token in `item`, if it was saved for exactly `origin`; otherwise "". */
export function tokenFor(item: string | null | undefined, origin: string): string {
	if (!item) return "";
	try {
		const v = JSON.parse(item) as { origin?: unknown; token?: unknown };
		return v && typeof v === "object" && v.origin === origin && typeof v.token === "string" ? v.token : "";
	} catch {
		// Not what this app writes (a bare token, another program's item): not for any address.
		return "";
	}
}
