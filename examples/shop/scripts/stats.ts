// Percentiles and a table row, shared by the timing and load scripts.
export function pct(sorted: number[], p: number): number {
  if (!sorted.length) return NaN;
  return sorted[Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length))];
}

export function ms(x: number): string {
  return x < 10 ? x.toFixed(2) : x < 100 ? x.toFixed(1) : x.toFixed(0);
}

/** A request timed from its start to the last byte of its body. */
export async function timed(url: string, init?: RequestInit): Promise<{ ms: number; status: number; bytes: number }> {
  const t = performance.now();
  const res = await fetch(url, init);
  const body = await res.arrayBuffer();
  return { ms: performance.now() - t, status: res.status, bytes: body.byteLength };
}
