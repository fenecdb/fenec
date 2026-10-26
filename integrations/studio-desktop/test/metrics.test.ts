import assert from "node:assert/strict";
import { test } from "node:test";
import { between, deadShare, formatBytes, formatCount, formatSeconds, readMetrics } from "../src/bun/metrics.ts";

// As fenec-server writes /_metrics (crates/fenec-http/src/metrics.rs), cut to what is read.
const text = (statements: number, buckets: number[]) => `# HELP fenec_documents Documents in a collection.
# TYPE fenec_documents gauge
fenec_documents{collection="orders"} 100000
fenec_documents{collection="customers"} 2500
fenec_data_bytes{collection="orders"} 8000000
fenec_file_bytes 8700000
fenec_reclaimable_bytes 348000
fenec_connections 3
fenec_connections_refused_total{reason="ceiling"} 0
fenec_statements_total{kind="read"} ${statements - 10}
fenec_statements_total{kind="write"} 10
${["0.0001", "0.00025", "0.0005", "0.001", "+Inf"].map((le, i) => `fenec_statement_duration_seconds_bucket{kind="read",le="${le}"} ${buckets[i]}`).join("\n")}
fenec_statement_duration_seconds_bucket{kind="write",le="0.001"} 0
fenec_statement_duration_seconds_bucket{kind="write",le="+Inf"} 10
fenec_statement_duration_seconds_sum{kind="read"} 0.5
`;

test("the figures a database's card shows, from its server's own metrics", () => {
	const s = readMetrics(text(110, [0, 10, 60, 100, 100]), 1000);
	assert.equal(s.collections, 2);
	assert.deepEqual(s.names, ["orders", "customers"]);
	assert.equal(s.rows, 102500);
	assert.equal(s.fileBytes, 8700000);
	assert.equal(s.reclaimable, 348000);
	assert.equal(s.connections, 3);
	assert.equal(s.statements, 110);
	assert.deepEqual(s.buckets.at(-1), [Infinity, 110], "every kind's buckets together");
	assert.equal(deadShare(s), "4%");
});

test("statements a second and the median answer, between two reads", () => {
	const a = readMetrics(text(110, [0, 10, 60, 100, 100]), 1000);
	assert.deepEqual(between(null, a), { perSecond: null, median: null }, "one read says nothing of a rate");
	// 200 more read statements in 2 s, all between 0.25 and 0.5 ms.
	const b = readMetrics(text(310, [0, 10, 260, 300, 300]), 3000);
	const r = between(a, b);
	assert.equal(r.perSecond, 100);
	assert.ok(r.median !== null && r.median > 0.00025 && r.median < 0.0005, String(r.median));
	assert.deepEqual(between(b, b), { perSecond: null, median: null });
	// A server that started again counts from 0: no rate across that.
	assert.deepEqual(between(b, readMetrics(text(20, [0, 0, 10, 10, 10]), 4000)), { perSecond: null, median: null });
	// Nothing answered in between: a rate of 0, no median.
	assert.deepEqual(between(b, readMetrics(text(310, [0, 10, 260, 300, 300]), 5000)), { perSecond: 0, median: null });
});

test("the words for bytes, times and counts", () => {
	assert.equal(formatBytes(41), "41 B");
	assert.equal(formatBytes(412_000), "412 KB");
	assert.equal(formatBytes(8_700_000), "8.7 MB");
	assert.equal(formatBytes(64_100_000), "64.1 MB");
	assert.equal(formatSeconds(0.0004), "0.4 ms");
	assert.equal(formatSeconds(0.012), "12 ms");
	assert.equal(formatSeconds(1.25), "1.3 s");
	assert.equal(formatCount(3400000), "3 400 000");
	assert.equal(deadShare({ fileBytes: 41, reclaimable: 41 }), "—", "a new file is its head: not dead space anyone acts on");
});
