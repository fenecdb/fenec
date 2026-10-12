'use client';
// The map a page draws its couriers and orders on: the city's ground (drawn
// on the server, `base`), and over it what `children` draws for the view's
// scale -- markers keep their size on screen as the map zooms. Dragged to
// pan, the wheel or the buttons to zoom; `focus` moves the view to a point.
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { HEIGHT, WIDTH } from '../lib/city';

type View = [x: number, y: number, w: number, h: number];

export function LiveMap({
  base,
  children,
  initial,
  focus,
  label,
  controls = true,
}: {
  base: ReactNode;
  children: (k: number) => ReactNode;
  initial?: View;
  /** A point to centre on, in the map's metres, when it changes. */
  focus?: [number, number] | null;
  label: string;
  controls?: boolean;
}) {
  const [view, setView] = useState<View>(initial ?? [0, 0, WIDTH, HEIGHT]);
  const svg = useRef<SVGSVGElement>(null);
  const drag = useRef<{ x: number; y: number; view: View } | null>(null);
  // A new focus moves the view, set while rendering rather than after:
  // React's way to adjust state to a prop that changed.
  const key = focus ? `${focus[0]},${focus[1]}` : '';
  const [focused, setFocused] = useState(key);
  if (key !== focused) {
    setFocused(key);
    if (focus) setView(([, , w, h]) => [focus[0] - w / 2, focus[1] - h / 2, w, h]);
  }
  const zoom = (f: number, cx?: number, cy?: number) =>
    setView(([x, y, w, h]) => {
      const nw = Math.min(WIDTH * 1.4, Math.max(600, w * f));
      const nh = (nw / w) * h;
      const ox = cx ?? x + w / 2;
      const oy = cy ?? y + h / 2;
      return [ox - ((ox - x) / w) * nw, oy - ((oy - y) / h) * nh, nw, nh];
    });
  useEffect(() => {
    const el = svg.current;
    if (!el) return;
    const wheel = (e: WheelEvent) => {
      e.preventDefault();
      const r = el.getBoundingClientRect();
      setView(([x, y, w, h]) => {
        const cx = x + ((e.clientX - r.left) / r.width) * w;
        const cy = y + ((e.clientY - r.top) / r.height) * h;
        const f = Math.exp(e.deltaY * 0.0015);
        const nw = Math.min(WIDTH * 1.4, Math.max(600, w * f));
        const nh = (nw / w) * h;
        return [cx - ((cx - x) / w) * nw, cy - ((cy - y) / h) * nh, nw, nh];
      });
    };
    el.addEventListener('wheel', wheel, { passive: false });
    return () => el.removeEventListener('wheel', wheel);
  }, []);
  const k = Math.pow(view[2] / WIDTH, 0.75);
  return (
    <div className="mapwrap">
      <svg
        ref={svg}
        viewBox={view.map((v) => Math.round(v)).join(' ')}
        preserveAspectRatio="xMidYMid slice"
        role="img"
        aria-label={label}
        onPointerDown={(e) => {
          if ((e.target as Element).closest('[data-pick]')) return;
          (e.currentTarget as SVGSVGElement).setPointerCapture(e.pointerId);
          drag.current = { x: e.clientX, y: e.clientY, view };
        }}
        onPointerMove={(e) => {
          const d = drag.current;
          if (!d || !svg.current) return;
          const r = svg.current.getBoundingClientRect();
          const s = Math.min(d.view[2] / r.width, d.view[3] / r.height);
          setView([d.view[0] - (e.clientX - d.x) * s, d.view[1] - (e.clientY - d.y) * s, d.view[2], d.view[3]]);
        }}
        onPointerUp={() => (drag.current = null)}
      >
        {base}
        {children(k)}
      </svg>
      {controls && (
        <div className="zoom">
          <button type="button" aria-label="Zoom in" onClick={() => zoom(0.7)}>
            +
          </button>
          <button type="button" aria-label="Zoom out" onClick={() => zoom(1 / 0.7)}>
            −
          </button>
        </div>
      )}
    </div>
  );
}

/** A courier: an arrow along its heading, its colour its state. */
export function CourierMark({ x, y, heading, state, k, title, mine, onPick }: { x: number; y: number; heading: number; state: string; k: number; title: string; mine?: boolean; onPick?: () => void }) {
  return (
    <g className={`courier ${state}${mine ? ' mine' : ''}`} style={{ transform: `translate(${x}px, ${y}px)` }} data-pick onClick={onPick}>
      <g transform={`scale(${k}) rotate(${heading})`}>
        <path d="M0 -70 L48 52 L0 28 L-48 52 Z" />
      </g>
      <title>{title}</title>
    </g>
  );
}

/** Where an order is picked up: a ticket, its colour its state. */
export function OrderPin({ x, y, state, k, title, selected, onPick }: { x: number; y: number; state: string; k: number; title: string; selected?: boolean; onPick?: () => void }) {
  return (
    <g className={`pin ${state}${selected ? ' selected' : ''}`} data-pick onClick={onPick} style={{ cursor: onPick ? 'pointer' : undefined }}>
      <rect x={-32} y={-32} width={64} height={64} rx={10} transform={`translate(${x} ${y}) scale(${k})`} />
      <title>{title}</title>
    </g>
  );
}
