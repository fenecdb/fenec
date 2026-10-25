// What the Keychain holds for a saved server: the token **and the origin it was
// saved for**, as one value. A token read back is a token for that address
// alone: if the connection's address has changed since (an edit that landed
// between the read and the use, one that left the old item behind, an item
// written by anything else), the answer is "no token", and nothing is sent to
// the new address. The Keychain and `state.json` are two stores written one
// after the other, so no order of writing them makes their pair atomic; the
// binding does not need one. Pure, so `node --test` holds it (test/token.test.ts).

export const keychainName = (connectionId: string) => `connection:${connectionId}`;

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
