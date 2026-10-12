// Bars drawn on the server: on time at the base, late on top, a 2 px gap
// between them and the top end rounded. Each bar carries its numbers as a
// tooltip (`<title>`), and the legend names both: the two colours are told
// apart by place and label as well as hue.

export interface Stack {
  label: string;
  /** Shown under the bar, or none. */
  tick?: string;
  onTime: number;
  late: number;
}

const top = (x: number, y: number, w: number, h: number, r: number) => {
  const rr = Math.min(r, w / 2, h);
  return `M${x} ${y + h}V${y + rr}Q${x} ${y} ${x + rr} ${y}H${x + w - rr}Q${x + w} ${y} ${x + w} ${y + rr}V${y + h}Z`;
};

export function StackedBars({ data, title, height = 220, width = 760 }: { data: Stack[]; title: string; height?: number; width?: number }) {
  const pad = { l: 36, r: 8, t: 10, b: 26 };
  const max = Math.max(1, ...data.map((d) => d.onTime + d.late));
  const step = niceStep(max);
  const ymax = Math.ceil(max / step) * step;
  const plotH = height - pad.t - pad.b;
  const slot = (width - pad.l - pad.r) / Math.max(1, data.length);
  const bw = Math.max(2, Math.min(28, slot - 3));
  const y = (v: number) => pad.t + plotH - (v / ymax) * plotH;
  return (
    <svg className="chart" viewBox={`0 0 ${width} ${height}`} role="img" aria-label={title}>
      {Array.from({ length: Math.floor(ymax / step) + 1 }, (_, i) => i * step).map((v) => (
        <g key={v}>
          <line x1={pad.l} x2={width - pad.r} y1={y(v)} y2={y(v)} className="grid" />
          <text x={pad.l - 6} y={y(v) + 4} textAnchor="end" className="axis">
            {v}
          </text>
        </g>
      ))}
      {data.map((d, i) => {
        const x = pad.l + i * slot + (slot - bw) / 2;
        const hOn = (d.onTime / ymax) * plotH;
        const hLate = (d.late / ymax) * plotH;
        const base = pad.t + plotH;
        const gap = d.late > 0 && d.onTime > 0 ? 2 : 0;
        return (
          <g key={d.label}>
            <title>{`${d.label}: ${d.onTime + d.late} delivered, ${d.onTime} on time, ${d.late} late`}</title>
            <rect x={x - 1} y={pad.t} width={bw + 2} height={plotH} fill="transparent" />
            {d.onTime > 0 && (d.late > 0 ? <rect x={x} y={base - hOn} width={bw} height={hOn} className="bar-on" /> : <path d={top(x, base - hOn, bw, hOn, 4)} className="bar-on" />)}
            {d.late > 0 && <path d={top(x, base - hOn - gap - hLate, bw, hLate, 4)} className="bar-late" />}
            {d.tick && (
              <text x={x + bw / 2} y={height - 8} textAnchor="middle" className="axis">
                {d.tick}
              </text>
            )}
          </g>
        );
      })}
    </svg>
  );
}

function niceStep(max: number): number {
  const raw = max / 4;
  const mag = 10 ** Math.floor(Math.log10(raw));
  const n = raw / mag;
  return (n <= 1 ? 1 : n <= 2 ? 2 : n <= 5 ? 5 : 10) * mag;
}

export function Key() {
  return (
    <p className="key">
      <span>
        <i style={{ background: 'var(--on-time)' }} />
        On time
      </span>
      <span>
        <i style={{ background: 'var(--late-bar)' }} />
        Late
      </span>
    </p>
  );
}
