// What a running server says about itself, read from its own `/_metrics`
// (Prometheus text, readable with its token) and made into the few figures the
// Databases and Servers lists show: collections, rows, file size, dead space,
// connections, statements a second and the median answer. The app reads
// nothing of the file itself. Pure, so `node --test` holds it
// (test/metrics.test.ts).

export type Sample = {
	/** When it was read, in ms. */
	at: number;
	collections: number;
	/** The collections' names, in the order the server lists them. */
	names: string[];
	rows: number;
	fileBytes: number;
	/** What a compaction would give back: what `--auto-compact` judges by. */
	reclaimable: number;
	connections: number;
	/** Statements answered since the server started, every kind. */
	statements: number;
	/** The answer times' histogram, every kind together: [upper bound in s, cumulative count], ascending. */
	buckets: [number, number][];
};

const LINE = /^([a-z_:][a-z0-9_:]*)(\{[^}]*\})?\s+(\S+)/;
const LE = /le="([^"]*)"/;
const COLLECTION = /collection="((?:[^"\\]|\\.)*)"/;

export function readMetrics(text: string, at: number): Sample {
	const s: Sample = { at, collections: 0, names: [], rows: 0, fileBytes: 0, reclaimable: 0, connections: 0, statements: 0, buckets: [] };
	const buckets = new Map<number, number>();
	for (const line of text.split("\n")) {
		const m = LINE.exec(line);
		if (!m) continue;
		const [, name, labels = "", raw] = m;
		const v = Number(raw);
		if (!Number.isFinite(v) && raw !== "+Inf") continue;
		switch (name) {
			case "fenec_documents":
				s.collections += 1;
				s.names.push(COLLECTION.exec(labels)?.[1] ?? "");
				s.rows += v;
				break;
			case "fenec_file_bytes":
				s.fileBytes = v;
				break;
			case "fenec_reclaimable_bytes":
				s.reclaimable = v;
				break;
			case "fenec_connections":
				s.connections = v;
				break;
			case "fenec_statements_total":
				s.statements += v;
				break;
			case "fenec_statement_duration_seconds_bucket": {
				const le = LE.exec(labels)?.[1];
				const bound = le === "+Inf" ? Infinity : Number(le);
				if (!Number.isNaN(bound)) buckets.set(bound, (buckets.get(bound) ?? 0) + v);
				break;
			}
		}
	}
	s.buckets = [...buckets].sort((a, b) => a[0] - b[0]);
	return s;
}

/**
 * Statements a second and the median answer between two samples of one
 * server: `null` where there is no earlier sample, or nothing was answered in
 * between. The median is read from the histogram's buckets as Prometheus's
 * `histogram_quantile` reads it, linearly within the bucket it falls in.
 */
export function between(prev: Sample | null, cur: Sample): { perSecond: number | null; median: number | null } {
	if (!prev || cur.at <= prev.at || cur.statements < prev.statements) return { perSecond: null, median: null };
	const answered = cur.statements - prev.statements;
	const perSecond = answered / ((cur.at - prev.at) / 1000);
	const before = new Map(prev.buckets);
	const delta = cur.buckets.map(([le, n]) => [le, n - (before.get(le) ?? 0)] as const);
	const total = delta.length ? delta[delta.length - 1]![1] : 0;
	if (total <= 0) return { perSecond, median: null };
	const half = total / 2;
	let lower = 0;
	let below = 0;
	for (const [le, n] of delta) {
		if (n >= half) {
			// Past the last finite bound there is nothing to place it by: the bound below is what is known.
			if (!Number.isFinite(le)) return { perSecond, median: lower };
			const inside = n - below;
			return { perSecond, median: inside > 0 ? lower + (le - lower) * ((half - below) / inside) : le };
		}
		lower = le;
		below = n;
	}
	return { perSecond, median: null };
}

/** 8.7 MB, 412 KB, 41 B: decimal, as Finder counts. */
export function formatBytes(n: number): string {
	if (n < 1000) return `${n} B`;
	const units = ["KB", "MB", "GB", "TB"];
	let v = n / 1000;
	let i = 0;
	while (v >= 1000 && i < units.length - 1) {
		v /= 1000;
		i++;
	}
	return `${v >= 100 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}

/** 0.4 ms, 1.7 ms, 12 ms, 1.2 s. */
export function formatSeconds(s: number): string {
	const ms = s * 1000;
	if (ms < 10) return `${ms.toFixed(1)} ms`;
	if (ms < 1000) return `${Math.round(ms)} ms`;
	return `${(ms / 1000).toFixed(1)} s`;
}

/**
 * The share of the file a compaction would give back, as the card says it; a
 * small file is mostly its head and the schema's records, which are not dead
 * space anyone would act on, so below 64 KB it is not given.
 */
export function deadShare(s: Pick<Sample, "fileBytes" | "reclaimable">): string {
	if (s.fileBytes < 64 * 1024) return "—";
	return `${Math.round((100 * Math.min(s.reclaimable, s.fileBytes)) / s.fileBytes)}%`;
}

/** 1 234 567: digits grouped by a space, as the app's pages write them. */
export const formatCount = (n: number) => String(Math.round(n)).replace(/\B(?=(\d{3})+(?!\d))/g, " ");
