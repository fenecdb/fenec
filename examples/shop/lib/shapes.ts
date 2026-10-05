// A product's picture is drawn, not photographed: a silhouette of what it
// is over bands of dune in a tint of its colour, a few hundred bytes of SVG
// inline in the page. No image request, no layout shift, and the catalog's
// 10 000 pictures cost nothing to store. Each shape is drawn on a 120 x 120
// grid; `fill` paths take the product's colour, `line` paths the ink.

export const SHAPES: Record<string, { fill: string; line?: string }> = {
  tent: { fill: 'M14 92 60 26l46 66Z', line: 'M60 26v66M48 92l12-26 12 26' },
  bag: { fill: 'M40 22h40q10 0 10 12v54q0 12-10 12H40q-10 0-10-12V34q0-12 10-12Z', line: 'M38 40h44M60 22v78' },
  pad: { fill: 'M18 40h84v40H18Z', line: 'M18 50h84M18 60h84M18 70h84' },
  tarp: { fill: 'M14 44 106 30 96 70 22 80Z', line: 'M22 80v18M96 70v28' },
  pack: { fill: 'M38 30q22-14 44 0v62q0 8-8 8H46q-8 0-8-8Z', line: 'M44 52h32v24H44ZM46 30q14-10 28 0' },
  duffel: { fill: 'M16 52q0-14 14-14h60q14 0 14 14v26q0 10-10 10H26q-10 0-10-10Z', line: 'M42 38q18-16 36 0M16 60h88' },
  hydration: { fill: 'M40 24h40l6 70q0 6-6 6H40q-6 0-6-6Z', line: 'M86 40q16 6 12 30t-14 26M50 40h20' },
  cube: { fill: 'M22 38h76v48H22Z', line: 'M22 50h76M60 38v48' },
  bottle: { fill: 'M50 18h20v12q12 6 12 20v44q0 8-8 8H46q-8 0-8-8V50q0-14 12-20Z', line: 'M38 60h44' },
  filter: { fill: 'M46 18h28v20l8 10v44q0 8-8 8H46q-8 0-8-8V48l8-10Z', line: 'M46 56h28M46 68h28M46 80h28' },
  stove: { fill: 'M36 64h48l-6 18H42Z', line: 'M60 64V40M50 46q10-16 20 0M44 82l-8 16M76 82l8 16M30 64h60' },
  pot: { fill: 'M26 46h68v34q0 14-14 14H40q-14 0-14-14Z', line: 'M18 52h8M94 52h8M30 40h60' },
  cooler: { fill: 'M18 48h84v40q0 6-6 6H24q-6 0-6-6Z', line: 'M14 42h92v8H14ZM48 32h24v10' },
  headlamp: { fill: 'M46 44h28q8 0 8 8v14q0 8-8 8H46q-8 0-8-8V52q0-8 8-8Z', line: 'M38 58H18M82 58h20M60 52v12' },
  lantern: { fill: 'M40 36h40v52q0 8-8 8H48q-8 0-8-8Z', line: 'M48 36q12-18 24 0M40 52h40M40 72h40' },
  solar: { fill: 'M16 34h40v52H16ZM64 34h40v52H64Z', line: 'M16 51h40M16 68h40M64 51h40M64 68h40M36 34v52M84 34v52' },
  battery: { fill: 'M34 26h52q6 0 6 6v58q0 6-6 6H34q-6 0-6-6V32q0-6 6-6Z', line: 'M48 52h24M60 40v24M44 80h32' },
  hat: { fill: 'M12 78q48-16 96 0-12 8-48 8T12 78ZM36 74q0-34 24-34t24 34Z', line: 'M36 66h48' },
  scarf: { fill: 'M24 28h72l-12 18H36Z', line: 'M36 46 26 96M84 46l10 50M30 70h12M78 70h12' },
  glasses: { fill: 'M18 50h34v14q0 12-14 12h-6q-14 0-14-12ZM68 50h34v14q0 12-14 12h-6q-14 0-14-12Z', line: 'M52 54q8-6 16 0M18 50 8 44M102 50l10-6' },
  shirt: { fill: 'M44 22 26 30 12 50l16 10 8-10v48h48V50l8 10 16-10-14-20-18-8q-6 10-16 10t-16-10Z', line: 'M60 32v66' },
  trousers: { fill: 'M36 20h48l6 80H70l-10-56-10 56H30Z', line: 'M36 30h48' },
  boot: { fill: 'M36 18h26v46l30 12q10 4 10 14v6H26V24q0-6 10-6Z', line: 'M26 86h76M40 36h18M40 48h18' },
  sandal: { fill: 'M30 30q18-10 36 0l6 56q0 14-24 14T24 86Z', line: 'M30 46h40M28 62h44M48 30v14' },
  sock: { fill: 'M46 16h30v52l12 14q8 12-4 18H58q-12 0-12-14Z', line: 'M46 30h30' },
  jacket: { fill: 'M46 20 28 26 14 56l10 6 10-14v52h52V48l10 14 10-6-14-30-18-6q-4 12-14 12t-14-12Z', line: 'M60 32v68M46 20q14 10 28 0' },
  compass: { fill: 'M60 20a40 40 0 1 0 .1 0Z', line: 'M60 36l8 24-8 24-8-24ZM60 20v6M60 94v6M20 60h6M94 60h6' },
  aid: { fill: 'M22 38h76v52H22Z', line: 'M60 50v28M46 64h28M48 38v-8h24v8' },
  knife: { fill: 'M14 66 74 50q16-2 20 10l-80 6Z', line: 'M74 60h32q6 0 6 6t-6 6H72' },
  chair: { fill: 'M34 30h52l-6 34H40Z', line: 'M40 64 28 96M80 64l12 32M30 96h20M70 96h20M40 64h40' },
};

/** Each product colour's swatch: the silhouette's fill and the band tint. */
export const SWATCH: Record<string, string> = {
  sand: '#c7a876',
  indigo: '#2c3a7a',
  saffron: '#d99a1e',
  slate: '#5a6472',
  olive: '#6d6b3a',
  rust: '#9c4a26',
  bone: '#e3dccd',
  charcoal: '#35363a',
  'oasis green': '#2e7a64',
  'dusk blue': '#5b6f9c',
  clay: '#b9785a',
  sage: '#8fa38a',
};

function hash(s: string): number {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619) >>> 0;
  return h;
}

/**
 * The picture's parts, for a server component to draw as JSX and the
 * image route to write as a file: two dune ridges whose crests move with
 * the sku, so no two pictures in a grid repeat.
 */
export function art(shape: string, colour: string | null, sku: string) {
  const s = SHAPES[shape] ?? SHAPES.cube;
  const swatch = SWATCH[colour ?? ''] ?? '#c7a876';
  const h = hash(sku);
  const a = 70 + (h % 16);
  const b = 84 + ((h >>> 8) % 12);
  const c = 20 + ((h >>> 16) % 70);
  return {
    swatch,
    dunes: [
      `M0 ${a}Q${c} ${a - 14} ${c + 30} ${a}T120 ${a - 4}V120H0Z`,
      `M0 ${b}Q${120 - c} ${b - 10} ${150 - c} ${b}T120 ${b + 2}V120H0Z`,
    ],
    fill: s.fill,
    line: s.line,
  };
}

/** The same picture as a standalone SVG file (JSON-LD and Open Graph). */
export function svgFile(shape: string, colour: string | null, sku: string): string {
  const p = art(shape, colour, sku);
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 120 120" width="1200" height="1200"><rect width="120" height="120" fill="#edefea"/><path d="${p.dunes[0]}" fill="${p.swatch}" opacity=".22"/><path d="${p.dunes[1]}" fill="${p.swatch}" opacity=".38"/><path d="${p.fill}" fill="${p.swatch}" stroke="#1e2758" stroke-width="2.5" stroke-linejoin="round"/>${p.line ? `<path d="${p.line}" fill="none" stroke="#1e2758" stroke-width="2.5" stroke-linecap="round"/>` : ''}</svg>`;
}
