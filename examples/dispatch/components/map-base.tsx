// Merrow drawn as a road atlas: sea and river, parks, the streets cased in
// grey and the arterials in signage yellow, their names along them, the
// districts. Static: rendered on the server once per page and handed to the
// live map as its ground, so the browser draws the streets without running
// the city's generator.
import { city, HEIGHT, WIDTH } from '../lib/city';

const Y = (y: number) => HEIGHT - y;
const r0 = (v: number) => Math.round(v);

let memo: { minor: string; major: string; bridges: string; coast: string; river: string; labels: { name: string; x: number; y: number; angle: number }[] } | null = null;

function paths() {
  if (memo) return memo;
  const c = city();
  let minor = '';
  let major = '';
  let bridges = '';
  // A street's name goes on its longest run of straight edges near the
  // middle of the map, once a street.
  const best = new Map<number, { len: number; x: number; y: number; angle: number }>();
  for (const e of c.edges) {
    const d = `M${r0(c.x[e.a])} ${r0(Y(c.y[e.a]))}L${r0(c.x[e.b])} ${r0(Y(c.y[e.b]))}`;
    const s = c.streets[e.street];
    if (e.bridge) bridges += d;
    else if (s.major) major += d;
    else minor += d;
    if (s.major && !e.bridge) {
      const mx = (c.x[e.a] + c.x[e.b]) / 2;
      const my = (c.y[e.a] + c.y[e.b]) / 2;
      const score = e.len - Math.abs(mx - WIDTH * 0.42) * 0.08 - Math.abs(my - HEIGHT * 0.55) * 0.08;
      const angle = (Math.atan2(-(c.y[e.b] - c.y[e.a]), c.x[e.b] - c.x[e.a]) * 180) / Math.PI;
      const upright = angle > 90 ? angle - 180 : angle < -90 ? angle + 180 : angle;
      const had = best.get(e.street);
      if (!had || score > had.len) best.set(e.street, { len: score, x: mx, y: Y(my), angle: upright });
    }
  }
  const coast = `M-60 ${HEIGHT + 60}` + c.coast.map(([x, y]) => `L${r0(x)} ${r0(Y(y))}`).join('') + `L${WIDTH + 60} ${HEIGHT + 60}Z`;
  const river = 'M' + c.river.map(([x, y]) => `${r0(x)} ${r0(Y(y))}`).join('L');
  const labels = [...best].map(([s, b]) => ({ name: c.streets[s].name, x: r0(b.x), y: r0(b.y), angle: Math.round(b.angle) }));
  memo = { minor, major, bridges, coast, river, labels };
  return memo;
}

export function MapBase() {
  const c = city();
  const p = paths();
  return (
    <g className="base" aria-hidden="true">
      <rect x={-200} y={-200} width={WIDTH + 400} height={HEIGHT + 400} className="land" />
      {c.parks.map((k) => (
        <g key={k.name}>
          <rect x={k.x + 20} y={Y(k.y + k.h) + 20} width={k.w - 40} height={k.h - 40} rx={40} className="park" />
          <text x={k.x + k.w / 2} y={Y(k.y + k.h / 2) + 18} className="park-name">
            {k.name}
          </text>
        </g>
      ))}
      <path d={p.coast} className="sea" />
      <path d={p.river} className="river" />
      <path d={p.minor} className="street-case" />
      <path d={p.bridges} className="bridge-case" />
      <path d={p.major} className="arterial-case" />
      <path d={p.minor} className="street" />
      <path d={p.bridges} className="street" />
      <path d={p.major} className="arterial" />
      {p.labels.map((l) => (
        <text key={l.name} className="street-name" transform={`translate(${l.x} ${l.y}) rotate(${-l.angle})`} dy={11}>
          {l.name}
        </text>
      ))}
      {c.districts.map((d) => (
        <text key={d.name} x={r0(d.x)} y={r0(Y(d.y))} className="district">
          {d.name}
        </text>
      ))}
    </g>
  );
}

export const VIEWBOX = `0 0 ${WIDTH} ${HEIGHT}`;
