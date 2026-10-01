/* The mark: a fennec drawn as a graph.

   Points joined by edges, as an index over vectors is, in the shape of the
   head: two tall ears, the eyes, the nose. No fill, no dots at the joints;
   the lines alone. When the page opens a light runs out from the tip of the
   right ear through every edge, a wave a step at a time, as a search spreads
   through a graph, and is gone in about a second and a half; hovering the
   mark runs it again. CSS animates it, so the mark costs no script, and it
   stands still for reduced motion.

   Every place the mark appears is built from this table: the header and the
   footer (`node site/fennec.js` writes `mark.svg`, the favicon, and
   `mark-detail.svg`, which `build.py` inlines) and the scenes that draw it on
   a canvas (`motion.js`). Coordinates are a 64 x 64 box. */

export const POINTS = {
  tl: [6, 4], tr: [58, 4],            // ear tips
  ol: [14, 32], or: [50, 32],         // where the ears meet the cheeks
  il: [26, 22], ir: [38, 22],         // where the ears meet the crown
  cr: [32, 20],                       // the crown
  el: [24, 37], er: [40, 37],         // the eyes
  cl: [17, 44], cr2: [47, 44],        // the cheeks
  no: [32, 49], ch: [32, 58],         // nose and chin
};

export const EDGES = [
  ['tl', 'ol'], ['tl', 'il'], ['il', 'cr'], ['cr', 'ir'], ['ir', 'tr'], ['tr', 'or'],
  ['ol', 'cl'], ['cl', 'ch'], ['ch', 'cr2'], ['cr2', 'or'],
  ['il', 'el'], ['ol', 'el'], ['el', 'no'], ['ir', 'er'], ['or', 'er'], ['er', 'no'],
  ['no', 'ch'], ['el', 'cl'], ['er', 'cr2'],
];

// Where the light starts.
export const START = 'tr';
export const STEP = 0.16;   // seconds between one wave of edges and the next
export const RUN = 0.3;     // seconds the light takes along one edge

/* Each edge, turned to run away from the start, and when its light leaves:
   its nearer end's distance from the start, in edges, times STEP. */
export const FLOW = (() => {
  const depth = { [START]: 0 }, queue = [START];
  while (queue.length) {
    const n = queue.shift();
    for (const [a, b] of EDGES) {
      const m = a === n ? b : b === n ? a : null;
      if (m && !(m in depth)) { depth[m] = depth[n] + 1; queue.push(m); }
    }
  }
  return EDGES.map(([a, b]) => {
    const [from, to] = depth[a] <= depth[b] ? [a, b] : [b, a];
    return { from, to, delay: depth[from] * STEP };
  });
})();
export const FLOW_LENGTH = Math.max(...FLOW.map((e) => e.delay)) + RUN;

export const COLORS = { line: '#F4A93C', light: '#4FE0C4' };

const at = (k) => POINTS[k].join(' ');
export const LINES_D = EDGES.map(([a, b]) => `M${at(a)}L${at(b)}`).join('');

/* The mark as SVG. `light` adds an edge for the CSS to run the light along
   each (`.mark-light`, each with its delay); `weight` thickens the lines for
   the favicon's 16 px. */
export function svg({ light = false, weight = 2 } = {}) {
  let body = `<path d="${LINES_D}" fill="none" stroke="${COLORS.line}" stroke-width="${weight}" stroke-linecap="round" stroke-linejoin="round"/>`;
  if (light) {
    body += `<g class="mark-light" fill="none" stroke="${COLORS.light}" stroke-width="${weight * 1.5}" stroke-linecap="round">`;
    for (const { from, to, delay } of FLOW) {
      body += `<path d="M${at(from)}L${at(to)}" pathLength="1" style="animation-delay:${delay.toFixed(2)}s"/>`;
    }
    body += '</g>';
  }
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" aria-hidden="true">${body}</svg>`;
}

// `node site/fennec.js` writes the two SVGs the generator uses.
if (typeof process !== 'undefined' && import.meta.url === `file://${process.argv[1]}`) {
  const { writeFileSync } = await import('node:fs');
  const here = new URL('.', import.meta.url).pathname;
  writeFileSync(here + 'mark.svg', svg({ weight: 3.2 }) + '\n');
  writeFileSync(here + 'mark-detail.svg', svg({ light: true }) + '\n');
}
