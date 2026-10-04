'use client';
// "Show more" without leaving the page. On the server it is a link to the
// next page, which is what a crawler or a page without JavaScript follows;
// once the page runs, a click fetches the next page's cards as JSON and
// draws them below, with the same card component the server used.
import { useState } from 'react';
import { ProductCard, type CardData } from './card';

export function LoadMore({ api, next }: { api: string; next: string }) {
  const [cards, setCards] = useState<CardData[]>([]);
  const [more, setMore] = useState<{ api: string; next: string } | null>({ api, next });
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState(false);

  async function load(e: React.MouseEvent) {
    if (!more || busy) return;
    e.preventDefault();
    setBusy(true);
    setFailed(false);
    try {
      const res = await fetch(more.api, { headers: { accept: 'application/json' } });
      if (!res.ok) throw new Error(String(res.status));
      const body = (await res.json()) as { cards: CardData[]; next: { api: string; next: string } | null };
      setCards((c) => [...c, ...body.cards]);
      setMore(body.next);
    } catch {
      setFailed(true);
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      {cards.length > 0 && (
        <ul className="grid more" aria-label="More results">
          {cards.map((p) => (
            <ProductCard key={p.sku} p={p} />
          ))}
        </ul>
      )}
      {more && (
        <p className="more">
          <a className="button quiet" href={more.next} onClick={load} aria-busy={busy}>
            {busy ? 'Loading…' : 'Show more'}
          </a>
          {failed && <span className="muted"> The next page did not load; the link opens it instead.</span>}
        </p>
      )}
    </>
  );
}
