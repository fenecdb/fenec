// The page shell every route is served, and the development mailbox. The
// pages themselves are drawn by public/app.js, which reads and writes the
// organisation straight from the database with the person's own token.
const esc = (s: string) => s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

const FONTS =
  'https://fonts.googleapis.com/css2?family=Atkinson+Hyperlegible+Next:wght@400;600;700&family=Bricolage+Grotesque:opsz,wdth,wght@12..96,75..100,500..800&display=swap';

export function shell(): string {
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Trellis</title>
<meta name="description" content="Trellis: boards, tasks and comments for small teams.">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" href="${FONTS}">
<link rel="stylesheet" href="/static/style.css">
<link rel="icon" href="/static/icon.svg" type="image/svg+xml">
<script type="module" src="/static/app.js"></script>
</head>
<body>
<div id="app" aria-live="polite"><p class="boot">Loading Trellis…</p></div>
<noscript><p class="boot">Trellis needs JavaScript to show your boards.</p></noscript>
</body>
</html>`;
}

export function mailPage(mails: { to: string; subject: string; body: string; at: string }[]): string {
  const link = (s: string) => esc(s).replace(/(https?:\/\/[^\s<]+)/g, '<a href="$1">$1</a>');
  const items = mails
    .map(
      (m) => `<article class="mail"><header><b>${esc(m.subject)}</b><span>to ${esc(m.to)}, ${esc(new Date(m.at).toLocaleString('en-GB'))}</span></header>
${m.body
  .split('\n\n')
  .map((p) => `<p>${link(p)}</p>`)
  .join('')}</article>`,
    )
    .join('');
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Development mailbox</title><link rel="stylesheet" href="/static/style.css"></head>
<body class="mailbox"><main><h1>Development mailbox</h1>
<p class="lede">What Trellis would have mailed, newest first. A deployment sends these through a mail service and has no such page.</p>
${items || '<p>No mail yet.</p>'}</main></body></html>`;
}
