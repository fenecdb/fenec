// Looking at a saved server before a window opens on it, to say what is wrong
// in words instead of showing a blank window or a sign-in form that cannot
// work. Pure but for `fetch`, which a test hands in (test/remote.test.ts).
//
// The studio is served by the server itself, before any token is asked, so
// three different questions have three different answers:
//   /_studio/   unreachable | not a page (the studio is off, or the server
//               is older than the studio) | the page
//   /_whoami    401: the token is refused (or none was given and one is needed)

export type Probe =
	| { kind: "ok" }
	| { kind: "unreachable"; reason: string }
	| { kind: "no-studio"; status: number }
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
	return u.origin;
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

export async function probeRemote(
	origin: string,
	token: string,
	tenant: string | null,
	fetchImpl: typeof fetch = fetch,
): Promise<Probe> {
	let studio: Response;
	try {
		studio = await fetchImpl(`${origin}/_studio/`, { signal: AbortSignal.timeout(PROBE_TIMEOUT_MS), redirect: "manual" });
	} catch (e) {
		return { kind: "unreachable", reason: describeNetworkError(e) };
	}
	await studio.body?.cancel();
	if (!(studio.ok && (studio.headers.get("content-type") ?? "").includes("html"))) return { kind: "no-studio", status: studio.status };

	const headers: Record<string, string> = token ? { authorization: `Bearer ${token}` } : {};
	const ask = async (path: string) => {
		const res = await fetchImpl(`${origin}${path}`, { headers, signal: AbortSignal.timeout(PROBE_TIMEOUT_MS) });
		let error = "";
		try {
			error = String(((await res.json()) as { error?: unknown })?.error ?? "");
		} catch {
			// Not JSON: the status says enough.
		}
		return { status: res.status, error };
	};
	try {
		let got = await ask("/_whoami");
		// A router answers 404 here and names the tenant in the path.
		if (got.status === 404 && tenant) got = await ask(`/t/${encodeURIComponent(tenant)}/_whoami`);
		if (got.status === 401) return { kind: "token-refused", detail: got.error || "invalid or missing token", hadToken: token !== "" };
	} catch (e) {
		return { kind: "unreachable", reason: describeNetworkError(e) };
	}
	return { kind: "ok" };
}
