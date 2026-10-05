// The tracker a site puts on its pages:
//
//   <script defer src="https://kestrel.example/k.js" data-site="fn4k7q2m9x"></script>
//
// It counts a pageview on load and on each client-side navigation, and
// `kestrel('signup_started', {plan: 'pro'})` counts an event of the site's
// own. Events wait up to five seconds, or until twenty, and go as one
// beacon: `fetch` with `keepalive`, sent again with the same batch id when
// it fails -- the server writes a batch once whatever the tries -- and
// `sendBeacon` when the page is hidden, which no page can wait on.
//
// The visitor is a random id kept in localStorage: no cookie, no
// fingerprint, nothing sent but the page, the referrer and the event.
// A body is text/plain, so a beacon needs no CORS preflight.
type Ev = { n: string; p: string; r?: string; t: number; d?: Record<string, string | number | boolean> };

((w: Window & { kestrel?: (n: string, d?: Ev['d']) => void }, d: Document) => {
  const s = d.currentScript as HTMLScriptElement | null;
  if (!s) return;
  const key = s.dataset.site;
  const url = new URL('/e', s.src).href;
  const id = () => {
    const a = new Uint8Array(12);
    crypto.getRandomValues(a);
    return btoa(String.fromCharCode(...a)).replace(/[+/=]/g, (c) => (c === '+' ? '-' : c === '/' ? '_' : ''));
  };
  let u: string;
  try {
    u = localStorage.k_id || (localStorage.k_id = id());
  } catch {
    u = id();
  }
  const q: Ev[] = [];
  let timer = 0;
  let ref = d.referrer;

  const post = (body: string, tries: number) =>
    fetch(url, { method: 'POST', body, keepalive: true })
      .then((r) => {
        if (r.status === 429 || r.status >= 500) throw 0;
      })
      .catch(() => tries < 4 && setTimeout(() => post(body, tries + 1), 1000 << tries));

  const flush = (hidden?: boolean) => {
    clearTimeout(timer);
    timer = 0;
    while (q.length) {
      const body = JSON.stringify({ k: key, b: id(), u, t: Date.now(), e: q.splice(0, 20) });
      if (!(hidden && navigator.sendBeacon?.(url, body))) post(body, 0);
    }
  };

  const track = (n: string, data?: Ev['d']) => {
    q.push({ n, p: location.pathname, r: n === 'pageview' ? ref : undefined, t: Date.now(), d: data });
    ref = '';
    if (q.length >= 20) flush();
    else timer ||= w.setTimeout(flush, 5000);
  };

  const push = history.pushState;
  history.pushState = function (...a: Parameters<History['pushState']>) {
    push.apply(this, a);
    track('pageview');
  };
  w.addEventListener('popstate', () => track('pageview'));
  d.addEventListener('visibilitychange', () => d.visibilityState === 'hidden' && flush(true));
  w.kestrel = track;
  track('pageview');
})(window, document);
