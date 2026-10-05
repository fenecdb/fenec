'use client';
// The stock line of a product page. The server writes it into the HTML
// (the page is kept for up to five minutes, so it may be a little old);
// once the browser is idle it asks for the number now, and from the
// shopper's first touch, scroll or key it follows the stock live:
// `FenecHttp.live` from `@fenecdb/web/client`, straight to fenec-server,
// with a token that reads `inventory` and nothing else (policy.txt).
//
// It polls (`{ poll }`) rather than holding a subscription: a stream is
// a thread on the server and a wake-up at every write, 64 of them by
// default, where a poll the server answers 304 while the stock has not
// moved holds nothing between rounds. Waiting for the first interaction
// still keeps a crawler, a page in a background tab or a load test from
// asking at all.
import { useEffect, useState } from 'react';

function line(n: number) {
  if (n <= 0) return { cls: 'stock out', text: 'Out of stock' };
  if (n <= 5) return { cls: 'stock low', text: `Only ${n} left` };
  return { cls: 'stock in', text: 'In stock, ships in 1 to 2 days' };
}

export function LiveStock({ sku, initial }: { sku: string; initial: number }) {
  const [n, setN] = useState(initial);

  useEffect(() => {
    let stopped = false;
    let stop: (() => void) | null = null;
    const now = async () => {
      const res = await fetch(`/api/stock/${encodeURIComponent(sku)}`, { cache: 'no-store' }).catch(() => null);
      if (res?.ok && !stopped) setN(((await res.json()) as { available: number }).available);
    };
    const hasIdle = typeof window.requestIdleCallback === 'function';
    const idle = hasIdle ? window.requestIdleCallback(() => void now()) : window.setTimeout(() => void now(), 1500);

    let tries = 0;
    const follow = async () => {
      if (stopped || tries++ > 3) return;
      const res = await fetch('/api/stock-token', { cache: 'no-store' }).catch(() => null);
      if (!res?.ok || stopped) return;
      const { url, token } = (await res.json()) as { url: string; token: string };
      const { connect } = await import('@fenecdb/web/client');
      if (stopped) return;
      const db = connect(url, { token });
      stop = db.live(db.from('inventory').select('available').where('sku', sku), (rows) => setN(Number(rows[0]?.available ?? 0)), {
        poll: 5000,
        // A token lapses after ten minutes: a round asked with it is
        // refused, so it is given up and followed again with a new one.
        onError: () => {
          stop?.();
          stop = null;
          window.setTimeout(() => void follow(), 1000 * tries);
        },
      });
    };
    const events = ['pointerdown', 'keydown', 'touchstart', 'scroll', 'mousemove'] as const;
    const first = () => {
      for (const e of events) window.removeEventListener(e, first);
      void follow();
    };
    for (const e of events) window.addEventListener(e, first, { once: true, passive: true });

    return () => {
      stopped = true;
      for (const e of events) window.removeEventListener(e, first);
      if (hasIdle) window.cancelIdleCallback(idle);
      else window.clearTimeout(idle);
      stop?.();
    };
  }, [sku]);

  const l = line(n);
  return (
    <p className={l.cls} aria-live="polite" data-available={n} style={{ margin: 0 }}>
      {l.text}
    </p>
  );
}
