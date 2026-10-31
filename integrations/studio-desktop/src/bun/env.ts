// What the programs the app starts (fenec-server, and the `fenec` shell that
// takes and restores backups) are given of the app's own environment: an
// allowlist, not "everything but ours". The server reads the environment for
// more than its token -- `OTEL_EXPORTER_OTLP_*` turns trace export on, a
// `FENEC_JWT_SECRET` changes what it accepts -- and `fenec` reads
// `FENEC_KEY_FILE` and `FENEC_REPLICATION_TOKEN`; a database file opened from
// a Finder double-click should do what it does for everyone, whatever the
// shell that launched the app had exported. The secrets the app hands over go
// here, never on a command line, which `ps` shows every user. Pure, so
// `node --test` holds it (test/env.test.ts).

const KEEP = new Set(["HOME", "PATH", "TMPDIR", "LANG", "USER", "LOGNAME", "TZ"]);

/** The secrets a program of the app's may be given, by the variable it reads them from. */
export type Secrets = {
	/** The data token (`fenec-server`). */
	FENEC_HTTP_TOKEN?: string;
	/** The replication token: `fenec-server` serves its stream with it, `fenec backup` asks with it. */
	FENEC_REPLICATION_TOKEN?: string;
};

export function childEnvironment(env: Record<string, string | undefined>, secrets: Secrets): Record<string, string> {
	const out: Record<string, string> = {};
	for (const [k, v] of Object.entries(env)) {
		if (v !== undefined && (KEEP.has(k) || k.startsWith("LC_"))) out[k] = v;
	}
	for (const [k, v] of Object.entries(secrets)) {
		// An empty one is none: the server and the shell take a variable set empty as unset.
		if (v) out[k] = v;
	}
	return out;
}

/** A server's environment: the allowlist, its data token and, unless none, its replication token. */
export function serverEnvironment(env: Record<string, string | undefined>, token: string, replicationToken: string | null = null): Record<string, string> {
	return childEnvironment(env, { FENEC_HTTP_TOKEN: token, ...(replicationToken ? { FENEC_REPLICATION_TOKEN: replicationToken } : {}) });
}
