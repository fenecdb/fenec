// Looking at a saved server before a window opens on it, to say what is wrong
// in words instead of showing a blank window or a sign-in form that cannot
// work. Pure but for `fetch`, which a test hands in (test/remote.test.ts).
//
// The studio is served by the server itself, before any token is asked, so
// three different questions have three different answers:
//   /_studio/   unreachable | not a page (the studio is off, or the server
//               is older than the studio) | the page
//   /_whoami    401: the token is refused (or none was given and one is needed)
//
// The token goes out last and only to a server that has shown it is one: it is
// asked for `/_whoami` without it first and sent only when fenecdb's own 401
// (a JSON body naming an error) comes back; and no request follows a redirect,
// since an `Authorization` header is sent wherever the redirect says, and a
// proxy in front of a server (a sign-in page, a moved address) would be told
// the token. A redirect is a message, not a place to go.

export type Probe =
	| { kind: "ok" }
	| { kind: "unreachable"; reason: string }
	| { kind: "no-studio"; status: number }
	| { kind: "redirected"; status: number; to: string }
	| { kind: "token-refused"; detail: string; hadToken: boolean };

export const PROBE_TIMEOUT_MS = 6000;

/** `http://host:port`, from what was typed: a bare `host:port` is plain http, as `curl` reads it. */
export function originOf(url: string): string {
	const typed = url.trim();
	if (typed === "") throw new Error("Type the server's address.");
	let u: URL;
	try {
		u = new URL(typed.includes("://") ? typed : `http://${typed}`);
	} catch {
		throw new Error(`"${typed}" is not an address. Write it as https://db.example.com or 127.0.0.1:8080.`);
	}
	if (u.protocol !== "http:" && u.protocol !== "https:") throw new Error("A server address starts with http:// or https://.");
	// The origin is the pattern a window's navigation rules are made of, where
	// `*` is a wildcard: a host holding one would let the window go anywhere that fits.
	if (/[*?]/.test(u.hostname)) throw new Error("An address cannot hold a * or a ?.");
	return u.origin;
}

/**
 * A saved server's token belongs to the address it was saved for. Pointing a
 * connection at another address with no token typed would send the old
 * server's to the new one: it may stay only where the origin is the same.
 */
export function tokenMayStay(storedOrigin: string, newOrigin: string, typedToken: string): boolean {
	return typedToken !== "" || storedOrigin === newOrigin;
}

/** True where the token would cross a network in the clear: plain http to anything but this machine. */
export function sendsTokenInTheClear(origin: string): boolean {
	const u = new URL(origin);
	if (u.protocol !== "http:") return false;
	const host = u.hostname.replace(/^\[|\]$/g, "");
	return !(host === "localhost" || host === "::1" || /^127\./.test(host));
}

const TLS = /^(CERT_|UNABLE_TO_|DEPTH_ZERO|SELF_SIGNED|ERR_TLS|Certificate)/;

/** What a failed `fetch` was, as a clause that finishes "Could not reach <server>: ...". */
export function describeNetworkError(e: unknown): string {
	const err = e as { code?: string; name?: string; message?: string; cause?: { code?: string; message?: string } };
	const code = err?.code ?? err?.cause?.code ?? "";
	// `AbortSignal.timeout` rejects with a TimeoutError, which has no code.
	if (err?.name === "TimeoutError" || err?.name === "AbortError" || code === "ETIMEDOUT") return `no answer in ${PROBE_TIMEOUT_MS / 1000} seconds`;
	if (code === "ECONNREFUSED" || code === "ConnectionRefused") return "nothing is listening at that address";
	if (code === "ENOTFOUND" || code === "EAI_AGAIN" || code === "DNSException") return "the name does not resolve";
	if (code === "EHOSTUNREACH" || code === "ENETUNREACH") return "the network cannot reach it";
	if (TLS.test(code)) return `its certificate is not accepted (${code})`;
	return code || err?.message || "unknown error";
}

type Answer = { status: number; error: string | null; redirect: string | null };

export async function probeRemote(
	origin: string,
	token: string,
	tenant: string | null,
	fetchImpl: typeof fetch = fetch,
): Promise<Probe> {
	/** Where a redirect points, as an origin: the path and query may hold what is not for a message. */
	const redirectTo = (res: Response) => {
		try {
			return new URL(res.headers.get("location") ?? "", origin).origin;
		} catch {
			return "another address";
		}
	};
	const ask = async (path: string, withToken: boolean): Promise<Answer> => {
		const res = await fetchImpl(`${origin}${path}`, {
			headers: withToken && token ? { authorization: `Bearer ${token}` } : {},
			redirect: "manual",
			signal: AbortSignal.timeout(PROBE_TIMEOUT_MS),
		});
		if (res.status >= 300 && res.status < 400) {
			await res.body?.cancel();
			return { status: res.status, error: null, redirect: redirectTo(res) };
		}
		let error: string | null = null;
		try {
			const body = (await res.json()) as { error?: unknown };
			if (typeof body?.error === "string") error = body.error;
		} catch {
			// Not JSON: not fenecdb's own answer.
		}
		return { status: res.status, error, redirect: null };
	};

	// The studio's page itself: served before any token, as HTML.
	let page: Response;
	try {
		page = await fetchImpl(`${origin}/_studio/`, { signal: AbortSignal.timeout(PROBE_TIMEOUT_MS), redirect: "manual" });
	} catch (e) {
		return { kind: "unreachable", reason: describeNetworkError(e) };
	}
	await page.body?.cancel();
	if (page.status >= 300 && page.status < 400) return { kind: "redirected", status: page.status, to: redirectTo(page) };
	if (!(page.ok && (page.headers.get("content-type") ?? "").includes("html"))) return { kind: "no-studio", status: page.status };

	/** `path` asked without the token, and with it only if fenecdb's own 401 says one is needed. */
	const gate = async (path: string): Promise<Probe | Answer> => {
		const anon = await ask(path, false);
		if (anon.redirect !== null) return { kind: "redirected", status: anon.status, to: anon.redirect };
		if (anon.status === 401 && anon.error !== null) {
			if (token === "") return { kind: "token-refused", detail: anon.error, hadToken: false };
			const sent = await ask(path, true);
			if (sent.redirect !== null) return { kind: "redirected", status: sent.status, to: sent.redirect };
			if (sent.status === 401) return { kind: "token-refused", detail: sent.error ?? "invalid or missing token", hadToken: true };
			return sent;
		}
		return anon;
	};
	const settled = (r: Probe | Answer): r is Probe => "kind" in r;
	try {
		let got = await gate("/_whoami");
		if (settled(got)) return got;
		// A router answers 404 here and names the tenant in the path.
		if (got.status === 404 && tenant) {
			got = await gate(`/t/${encodeURIComponent(tenant)}/_whoami`);
			if (settled(got)) return got;
		}
	} catch (e) {
		return { kind: "unreachable", reason: describeNetworkError(e) };
	}
	return { kind: "ok" };
}
