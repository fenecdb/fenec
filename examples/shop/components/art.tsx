import { art } from '../lib/shapes';

/**
 * A product's picture, inline: the silhouette of its kind over dunes in a
 * tint of its colour. Its box is fixed by `viewBox` and CSS, so it takes its
 * place before anything paints and never shifts the page.
 */
export function ProductArt({ shape, colour, sku, label, className }: { shape: string; colour: string | null; sku: string; label?: string; className?: string }) {
  const p = art(shape, colour, sku);
  return (
    <svg viewBox="0 0 120 120" className={className ?? 'art'} role={label ? 'img' : undefined} aria-label={label} aria-hidden={label ? undefined : true}>
      <path d={p.dunes[0]} fill={p.swatch} opacity=".22" />
      <path d={p.dunes[1]} fill={p.swatch} opacity=".38" />
      <path d={p.fill} fill={p.swatch} stroke="var(--ink)" strokeWidth="2.5" strokeLinejoin="round" />
      {p.line && <path d={p.line} fill="none" stroke="var(--ink)" strokeWidth="2.5" strokeLinecap="round" />}
    </svg>
  );
}
