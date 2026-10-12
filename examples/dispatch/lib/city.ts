// Merrow, the city Plover's couriers ride through: invented, and the same
// on every machine. Its streets are a graph made from a seed -- a grid bent
// out of true, a river with bridges, a coast, parks -- laid over real
// degrees, so that `distance` and `near` answer in real metres. The map is
// drawn from the same graph (components/city-map.tsx), the couriers ride
// its edges (scripts/simulate.ts), and every address is a point on one.
//
// Why not real map tiles: the tests and CI run with no network, a tile
// server's usage policy forbids a simulator's bulk requests, and moving
// couriers along streets needs the streets themselves, which a tile is a
// picture of. This module is plain TypeScript with no imports, so the
// browser draws the same city the server routes through.

/** The city's south-west corner, in degrees. */
const LON0 = 26.95;
const LAT0 = 38.33;
const M_PER_DEG_LAT = 110_540;
const M_PER_DEG_LON = 111_320 * Math.cos((LAT0 * Math.PI) / 180);

export const COLS = 46;
export const ROWS = 32;
const DX = 155;
const DY = 145;
/** The city's extent, metres. */
export const WIDTH = (COLS - 1) * DX + 200;
export const HEIGHT = (ROWS - 1) * DY + 200;
const PAD = 100;

export type Point = [lon: number, lat: number];

export interface Street {
  name: string;
  major: boolean;
}

export interface City {
  /** Node positions, metres from the south-west corner; NaN where there is none. */
  x: Float64Array;
  y: Float64Array;
  /** Each node's neighbours: [node, edge] pairs. */
  adj: [number, number][][];
  /** Edges: endpoints, length in metres, street. */
  edges: { a: number; b: number; len: number; street: number; bridge: boolean }[];
  streets: Street[];
  parks: { x: number; y: number; w: number; h: number; name: string }[];
  /** The coast and the river's centre line, sampled. */
  coast: [number, number][];
  river: [number, number][];
  districts: { name: string; x: number; y: number }[];
}

// ------------------------------------------------------------- the seed

/** mulberry32: a small PRNG, the same numbers everywhere. */
export function rng(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const coastY = (x: number) => 260 + 170 * Math.sin(x / 900) + 90 * Math.sin(x / 370 + 1.3);
const riverY = (x: number) => 2350 + 520 * Math.sin(x / 1500 + 0.4) + 120 * Math.sin(x / 430 + 2);
const RIVER_HALF = 40;

const NAMES_EW = [
  'Saltpan', 'Cooper', 'Weaver', 'Lantern', 'Chandler', 'Rope', 'Tanner', 'Mill', 'Granary', 'Ferry',
  'Cistern', 'Orchard', 'Quarry', 'Tallow', 'Bellfounder', 'Glazier', 'Fuller', 'Smithy', 'Mason', 'Vintner',
  'Thatcher', 'Potter', 'Cutler', 'Brewer', 'Dyer', 'Saddler', 'Wheelwright', 'Barrow', 'Chalk', 'Flint',
  'Juniper', 'Linden',
];
const NAMES_NS = [
  'Almond', 'Bramble', 'Cedar', 'Damson', 'Elder', 'Fennel', 'Gorse', 'Hazel', 'Ivy', 'Jasmine',
  'Kale', 'Laurel', 'Myrtle', 'Nettle', 'Olive', 'Poplar', 'Quince', 'Rowan', 'Sorrel', 'Thyme',
  'Umber', 'Vetch', 'Willow', 'Yarrow', 'Zinnia', 'Aspen', 'Birch', 'Clover', 'Dogwood', 'Ember',
  'Fig', 'Ginger', 'Holly', 'Iris', 'Jute', 'Kelp', 'Lilac', 'Maple', 'Nutmeg', 'Oak',
  'Pepper', 'Reed', 'Sage', 'Teasel', 'Ulex', 'Violet',
];

const PARKS = [
  { i: 8, j: 21, w: 4, h: 3, name: 'Kestle Park' },
  { i: 27, j: 23, w: 3, h: 4, name: 'Common' },
  { i: 36, j: 9, w: 4, h: 3, name: 'Ropewalk Gardens' },
  { i: 15, j: 6, w: 3, h: 2, name: 'Fuller Green' },
  { i: 40, j: 26, w: 3, h: 3, name: 'Heath' },
];

let made: City | null = null;

/** The city: made once from its seed. */
export function city(): City {
  if (made) return made;
  const r = rng(20261012);
  const n = COLS * ROWS;
  const x = new Float64Array(n).fill(NaN);
  const y = new Float64Array(n).fill(NaN);
  const inPark = (i: number, j: number) =>
    PARKS.some((p) => i > p.i && i < p.i + p.w && j > p.j && j < p.j + p.h);
  // The old town, north of the river near the middle, is bent further out
  // of true than the rest.
  const oldX = WIDTH * 0.46;
  const oldY = 3000;
  for (let j = 0; j < ROWS; j++) {
    for (let i = 0; i < COLS; i++) {
      const bx = PAD + i * DX;
      const by = PAD + j * DY;
      const old = Math.hypot(bx - oldX, by - oldY) < 800;
      const jit = old ? 34 : 16;
      const nx = bx + (r() - 0.5) * 2 * jit + 30 * Math.sin(by / 700);
      const ny = by + (r() - 0.5) * 2 * jit + 25 * Math.sin(bx / 900);
      if (ny < coastY(nx) + 40) continue; // the sea
      if (Math.abs(ny - riverY(nx)) < RIVER_HALF + 18) continue; // the river
      if (inPark(i, j)) continue;
      x[j * COLS + i] = nx;
      y[j * COLS + i] = ny;
    }
  }
  const streets: Street[] = [];
  const rowStreet: number[] = [];
  const colStreet: number[] = [];
  for (let j = 0; j < ROWS; j++) {
    const major = j % 6 === 3;
    rowStreet.push(streets.length);
    streets.push({ name: `${NAMES_EW[j % NAMES_EW.length]} ${major ? 'Boulevard' : 'Street'}`, major });
  }
  for (let i = 0; i < COLS; i++) {
    const major = i % 7 === 2;
    colStreet.push(streets.length);
    streets.push({ name: `${NAMES_NS[i % NAMES_NS.length]} ${major ? 'Avenue' : 'Road'}`, major });
  }
  const edges: City['edges'] = [];
  const exists = (k: number) => !Number.isNaN(x[k]);
  const add = (a: number, b: number, street: number, bridge: boolean) => {
    edges.push({ a, b, len: Math.hypot(x[a] - x[b], y[a] - y[b]), street, bridge });
  };
  for (let j = 0; j < ROWS; j++) {
    for (let i = 0; i < COLS; i++) {
      const k = j * COLS + i;
      if (!exists(k)) continue;
      // East: a street along the row, a few left out so blocks differ.
      if (i + 1 < COLS && exists(k + 1)) {
        const major = streets[rowStreet[j]].major;
        if (major || r() > 0.07) add(k, k + 1, rowStreet[j], false);
      }
      // North: along the column; across the river only at a bridge.
      if (j + 1 < ROWS) {
        const up = k + COLS;
        if (exists(up)) {
          const major = streets[colStreet[i]].major;
          if (major || r() > 0.07) add(k, up, colStreet[i], false);
        } else {
          // A bridge: the river lies between this node and the next one
          // north that exists, on a major avenue or every fifth column.
          let jj = j + 1;
          while (jj < ROWS && !exists(jj * COLS + i)) jj++;
          const far = jj * COLS + i;
          const crosses = jj < ROWS && y[k] < riverY(x[k]) && y[far] > riverY(x[far]) && jj - j <= 2;
          if (crosses && (streets[colStreet[i]].major || i % 5 === 0)) add(k, far, colStreet[i], true);
        }
      }
    }
  }
  // Keep the largest connected part: a courier can reach every address.
  const adj: [number, number][][] = Array.from({ length: n }, () => []);
  edges.forEach((e, ei) => {
    adj[e.a].push([e.b, ei]);
    adj[e.b].push([e.a, ei]);
  });
  const part = new Int32Array(n).fill(-1);
  let best = -1;
  let bestSize = 0;
  for (let s = 0; s < n; s++) {
    if (!exists(s) || part[s] >= 0) continue;
    const stack = [s];
    part[s] = s;
    let size = 0;
    while (stack.length) {
      const v = stack.pop()!;
      size++;
      for (const [w] of adj[v]) {
        if (part[w] >= 0) continue;
        part[w] = s;
        stack.push(w);
      }
    }
    if (size > bestSize) {
      bestSize = size;
      best = s;
    }
  }
  const keep = edges.map((e) => part[e.a] === best);
  const kept = edges.filter((_, i) => keep[i]);
  for (let k = 0; k < n; k++) {
    if (part[k] === best) continue;
    x[k] = NaN;
    y[k] = NaN;
  }
  const adj2: [number, number][][] = Array.from({ length: n }, () => []);
  kept.forEach((e, ei) => {
    adj2[e.a].push([e.b, ei]);
    adj2[e.b].push([e.a, ei]);
  });
  const coast: [number, number][] = [];
  const river: [number, number][] = [];
  for (let sx = -50; sx <= WIDTH + 50; sx += 40) {
    coast.push([sx, coastY(sx)]);
    river.push([sx, riverY(sx)]);
  }
  const parks = PARKS.map((p) => ({
    x: PAD + p.i * DX,
    y: PAD + p.j * DY,
    w: p.w * DX,
    h: p.h * DY,
    name: p.name,
  }));
  const districts = [
    { name: 'Old Town', x: oldX, y: oldY + 120 },
    { name: 'Harbour', x: WIDTH * 0.3, y: 700 },
    { name: 'Saltings', x: WIDTH * 0.78, y: 820 },
    { name: 'Northgate', x: WIDTH * 0.5, y: HEIGHT - 380 },
    { name: 'Mill End', x: WIDTH * 0.13, y: 3500 },
    { name: 'Eastfield', x: WIDTH * 0.84, y: 3600 },
    { name: 'Riverside', x: WIDTH * 0.66, y: 1900 },
    { name: 'Tanners', x: WIDTH * 0.16, y: 1500 },
  ];
  made = { x, y, adj: adj2, edges: kept, streets, parks, coast, river, districts };
  return made;
}

// ----------------------------------------------------------- coordinates

/** Metres from the south-west corner to degrees. */
export function toPoint(mx: number, my: number): Point {
  return [round6(LON0 + mx / M_PER_DEG_LON), round6(LAT0 + my / M_PER_DEG_LAT)];
}

/** Degrees to metres from the south-west corner. */
export function toMetres([lon, lat]: Point): [number, number] {
  return [(lon - LON0) * M_PER_DEG_LON, (lat - LAT0) * M_PER_DEG_LAT];
}

const round6 = (v: number) => Math.round(v * 1e6) / 1e6;

/** The metres between two points on fenecdb's sphere, as `distance()` answers them. */
export function metresBetween(a: Point, b: Point): number {
  const R = 6_372_797.560856;
  const rad = Math.PI / 180;
  const dLat = (b[1] - a[1]) * rad;
  const dLon = (b[0] - a[0]) * rad;
  const h = Math.sin(dLat / 2) ** 2 + Math.cos(a[1] * rad) * Math.cos(b[1] * rad) * Math.sin(dLon / 2) ** 2;
  return 2 * R * Math.asin(Math.min(1, Math.sqrt(h)));
}

/** The city's box, `[west, south, east, north]`, as `within` takes one. */
export function bounds(): [number, number, number, number] {
  const sw = toPoint(0, 0);
  const ne = toPoint(WIDTH, HEIGHT);
  return [sw[0], sw[1], ne[0], ne[1]];
}

// -------------------------------------------------------------- addresses

export interface Address {
  label: string;
  street: string;
  number: number;
  /** The door, metres: beside the street. */
  x: number;
  y: number;
  /** On the street, for a courier to stop at. */
  point: Point;
  /** The edge it lies on. */
  edge: number;
}

let doors: Address[] | null = null;

/**
 * Every door in the city: one each 22 metres along each side of every
 * street segment, numbered along its street, odd on one side and even on
 * the other. About 34 000.
 */
export function addresses(): Address[] {
  if (doors) return doors;
  const c = city();
  const out: Address[] = [];
  const along = new Map<number, number>(); // a street's next number
  c.edges.forEach((e, ei) => {
    if (e.bridge) return;
    const ax = c.x[e.a];
    const ay = c.y[e.a];
    const dx = (c.x[e.b] - ax) / e.len;
    const dy = (c.y[e.b] - ay) / e.len;
    const st = c.streets[e.street];
    let base = along.get(e.street) ?? 0;
    for (let d = 14; d < e.len - 10; d += 22) {
      base++;
      for (const side of [1, -1]) {
        const px = ax + dx * d;
        const py = ay + dy * d;
        const num = base * 2 - (side > 0 ? 1 : 0);
        out.push({
          label: `${num} ${st.name}`,
          street: st.name,
          number: num,
          x: px - dy * 9 * side,
          y: py + dx * 9 * side,
          point: toPoint(px, py),
          edge: ei,
        });
      }
    }
    along.set(e.street, base);
  });
  doors = out;
  return out;
}

// ---------------------------------------------------------------- routing

/** The node nearest a point, among the grid's cells around it. */
export function nearestNode(mx: number, my: number): number {
  const c = city();
  const ci = Math.round((mx - PAD) / DX);
  const cj = Math.round((my - PAD) / DY);
  let best = -1;
  let bestD = Infinity;
  for (let ring = 1; best < 0 || ring <= 2; ring++) {
    for (let j = Math.max(0, cj - ring); j <= Math.min(ROWS - 1, cj + ring); j++) {
      for (let i = Math.max(0, ci - ring); i <= Math.min(COLS - 1, ci + ring); i++) {
        const k = j * COLS + i;
        if (Number.isNaN(c.x[k]) || !c.adj[k].length) continue;
        const d = Math.hypot(c.x[k] - mx, c.y[k] - my);
        if (d < bestD) {
          bestD = d;
          best = k;
        }
      }
    }
    if (ring > Math.max(COLS, ROWS)) break;
  }
  return best;
}

/**
 * The way along the streets from one point to another: A* over the graph
 * from the node nearest each, the points themselves at either end. The
 * points in degrees, and the length in metres.
 */
export function route(from: Point, to: Point): { path: Point[]; metres: number } {
  const c = city();
  const [fx, fy] = toMetres(from);
  const [tx, ty] = toMetres(to);
  const s = nearestNode(fx, fy);
  const t = nearestNode(tx, ty);
  const n = c.x.length;
  const g = new Float64Array(n).fill(Infinity);
  const prev = new Int32Array(n).fill(-1);
  const h = (k: number) => Math.hypot(c.x[k] - c.x[t], c.y[k] - c.y[t]);
  const heap: [number, number][] = [];
  const push = (f: number, k: number) => {
    heap.push([f, k]);
    let i = heap.length - 1;
    while (i > 0) {
      const p = (i - 1) >> 1;
      if (heap[p][0] <= heap[i][0]) break;
      [heap[p], heap[i]] = [heap[i], heap[p]];
      i = p;
    }
  };
  const pop = (): [number, number] => {
    const top = heap[0];
    const last = heap.pop()!;
    if (heap.length) {
      heap[0] = last;
      let i = 0;
      for (;;) {
        const l = 2 * i + 1;
        const r = l + 1;
        let m = i;
        if (l < heap.length && heap[l][0] < heap[m][0]) m = l;
        if (r < heap.length && heap[r][0] < heap[m][0]) m = r;
        if (m === i) break;
        [heap[m], heap[i]] = [heap[i], heap[m]];
        i = m;
      }
    }
    return top;
  };
  g[s] = 0;
  push(h(s), s);
  while (heap.length) {
    const [, v] = pop();
    if (v === t) break;
    for (const [w, ei] of c.adj[v]) {
      const ng = g[v] + c.edges[ei].len;
      if (ng < g[w]) {
        g[w] = ng;
        prev[w] = v;
        push(ng + h(w), w);
      }
    }
  }
  const nodes: number[] = [];
  for (let k = t; k >= 0; k = prev[k]) {
    nodes.push(k);
    if (k === s) break;
  }
  nodes.reverse();
  const path: Point[] = [from, ...nodes.map((k) => toPoint(c.x[k], c.y[k])), to];
  let metres = 0;
  for (let i = 1; i < path.length; i++) metres += metresBetween(path[i - 1], path[i]);
  return { path, metres };
}

// -------------------------------------------------------------- drawing

/** A point where the map draws it: metres, y down, in a viewBox of WIDTH x HEIGHT. */
export function svgOf(p: Point): [number, number] {
  const [x, y] = toMetres(p);
  return [Math.round(x * 10) / 10, Math.round((HEIGHT - y) * 10) / 10];
}

/** A path's `d` through points. */
export function pathOf(points: Point[]): string {
  return points.map((p, i) => `${i ? 'L' : 'M'}${svgOf(p).join(' ')}`).join('');
}

/** The point `d` metres along a path, and the heading there (degrees from north). */
export function along(path: Point[], d: number): { at: Point; heading: number; done: boolean } {
  let left = d;
  for (let i = 1; i < path.length; i++) {
    const seg = metresBetween(path[i - 1], path[i]);
    if (left <= seg || i === path.length - 1) {
      const f = seg > 0 ? Math.min(1, left / seg) : 1;
      const a = path[i - 1];
      const b = path[i];
      const at: Point = [round6(a[0] + (b[0] - a[0]) * f), round6(a[1] + (b[1] - a[1]) * f)];
      const [ax, ay] = toMetres(a);
      const [bx, by] = toMetres(b);
      const heading = ((Math.atan2(bx - ax, by - ay) * 180) / Math.PI + 360) % 360;
      return { at, heading: Math.round(heading), done: left >= seg && i === path.length - 1 };
    }
    left -= seg;
  }
  return { at: path[path.length - 1], heading: 0, done: true };
}
