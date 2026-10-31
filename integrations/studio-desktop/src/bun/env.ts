// What the server the app starts is given of the app's own environment: an
// allowlist, not "everything but ours". The server reads the environment for
// more than its token -- `OTEL_EXPORTER_OTLP_*` turns trace export on, a
// `FENEC_JWT_SECRET` changes what it accepts -- and a database file opened
// from a Finder double-click should do what it does for everyone, whatever
// the shell that launched the app had exported. Pure, so `node --test` holds
// it (test/env.test.ts).

const KEEP = new Set(["HOME", "PATH", "TMPDIR", "LANG", "USER", "LOGNAME", "TZ"]);

export function serverEnvironment(env: Record<string, string | undefined>, token: string): Record<string, string> {
	const out: Record<string, string> = {};
	for (const [k, v] of Object.entries(env)) {
		if (v !== undefined && (KEEP.has(k) || k.startsWith("LC_"))) out[k] = v;
	}
	out["FENEC_HTTP_TOKEN"] = token;
	return out;
}
