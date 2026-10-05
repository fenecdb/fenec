// The console's pages, rendered on the server as strings. No script runs
// in the browser: forms post, links get, and every page is the database's
// answer under the signed-in person's own token.
import { formatAmount, symbol } from './money.ts';
import type { Report } from './reconcile.ts';
import type { User } from './users.ts';

export const esc = (v: unknown): string =>
  String(v ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]!);

/** A figure, right-aligned by its cell; a negative in red, in parentheses. */
const money = (minor: number, currency?: string) =>
  `<span class="fig${minor < 0 ? ' neg' : ''}">${currency ? `<span class="cur">${esc(symbol(currency))}</span>` : ''}${formatAmount(minor)}</span>`;

const when = (iso: unknown) => {
  const d = new Date(String(iso));
  if (Number.isNaN(d.getTime())) return '';
  return `<time datetime="${esc(iso)}">${d.toISOString().slice(0, 10)} ${d.toISOString().slice(11, 16)}</time>`;
};

const CSS = `
:root{--paper:#eef2ec;--sheet:#fbfcf9;--ink:#15232a;--muted:#56665f;--rule:#cfdccf;--rule2:#9db3a3;--red:#ad2c25;--green:#1d5c47;--green-ink:#fff;--warn:#8a5a00;--focus:#1d5c47}
@media (prefers-color-scheme:dark){:root:not([data-theme=light]){--paper:#0f1719;--sheet:#152022;--ink:#e1eae4;--muted:#91a29b;--rule:#263734;--rule2:#3f5a52;--red:#ec8478;--green:#73c4a2;--green-ink:#0f1719;--warn:#e2b25c;--focus:#73c4a2}}
:root[data-theme=dark]{--paper:#0f1719;--sheet:#152022;--ink:#e1eae4;--muted:#91a29b;--rule:#263734;--rule2:#3f5a52;--red:#ec8478;--green:#73c4a2;--green-ink:#0f1719;--warn:#e2b25c;--focus:#73c4a2}
*{box-sizing:border-box}
html{-webkit-text-size-adjust:100%}
body{margin:0;background:var(--paper);color:var(--ink);font:14px/1.45 system-ui,-apple-system,"Segoe UI",Roboto,sans-serif;font-variant-numeric:tabular-nums}
a{color:var(--green);text-underline-offset:2px}
:focus-visible{outline:2px solid var(--focus);outline-offset:2px}
.bar{display:flex;align-items:baseline;gap:24px;flex-wrap:wrap;padding:14px max(16px,calc((100vw - 1120px)/2));border-bottom:1px solid var(--rule2);background:var(--sheet)}
.mark{font-weight:700;font-size:17px;letter-spacing:-.01em;color:var(--ink);text-decoration:none}
.mark small{font-weight:400;color:var(--muted);font-size:13px;margin-left:8px}
nav{display:flex;gap:18px;flex-wrap:wrap}
nav a{color:var(--muted);text-decoration:none;padding-bottom:3px;border-bottom:2px solid transparent}
nav a[aria-current=page]{color:var(--ink);border-color:var(--green)}
.who{margin-left:auto;color:var(--muted);display:flex;gap:12px;align-items:baseline}
.who form{display:inline}
main{max-width:1120px;margin:0 auto;padding:24px 16px 64px}
h1{font-size:22px;font-weight:650;letter-spacing:-.015em;margin:0 0 4px}
h2{font-size:15px;font-weight:650;margin:32px 0 8px}
.lede{color:var(--muted);margin:0 0 20px;max-width:68ch}
.sheet{background:var(--sheet);border:1px solid var(--rule);overflow-x:auto}
table{border-collapse:collapse;width:100%}
th,td{padding:6px 12px;text-align:left;border-bottom:1px solid var(--rule);white-space:nowrap}
th{font-weight:600;color:var(--muted);font-size:12.5px;border-bottom-color:var(--rule2)}
td.n,th.n{text-align:right;border-left:1px solid var(--rule)}
td.wrap{white-space:normal;min-width:16ch}
tbody tr:hover{background:color-mix(in srgb,var(--rule) 30%,transparent)}
.fig{font-variant-numeric:tabular-nums lining-nums}
.fig .cur{color:var(--muted);margin-right:2px}
.neg{color:var(--red)}
.id{font-size:12.5px;color:var(--muted)}
.tag{font-size:12px;padding:1px 6px;border:1px solid var(--rule2);border-radius:2px;color:var(--muted)}
.tag.frozen{color:var(--red);border-color:currentColor}
tfoot td{font-weight:650;border-bottom:3px double var(--ink);border-top:1px solid var(--ink)}
.flash{padding:10px 14px;margin:0 0 20px;border-left:3px solid var(--green);background:var(--sheet)}
.flash.err{border-color:var(--red)}
form.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:12px;align-items:end;background:var(--sheet);border:1px solid var(--rule);padding:16px}
label{display:grid;gap:4px;color:var(--muted);font-size:12.5px}
input,select{font:inherit;color:var(--ink);background:var(--paper);border:1px solid var(--rule2);border-radius:2px;padding:7px 8px;min-width:0;width:100%}
input.amt{text-align:right}
input.refund{width:84px;padding:3px 6px}
td form.inline{display:flex;gap:4px;align-items:center}
button{font:inherit;font-weight:600;color:var(--green-ink);background:var(--green);border:1px solid var(--green);border-radius:2px;padding:7px 14px;cursor:pointer}
button.quiet{color:var(--green);background:transparent;padding:3px 8px;font-weight:500}
button.danger{color:var(--muted);border-color:transparent;background:transparent;padding:3px 8px;font-weight:500}
button.danger:hover,button.danger:focus-visible{color:var(--red);border-color:currentColor}
.inline{display:inline}
.two{display:grid;grid-template-columns:1fr 1fr;gap:24px}
.verdict{display:flex;gap:16px;align-items:baseline;margin:0 0 20px}
.verdict strong{font-size:18px}
.ok{color:var(--green)}.bad{color:var(--red)}
.facts{display:flex;flex-wrap:wrap;gap:4px 28px;color:var(--muted);margin:0 0 16px}
.facts b{color:var(--ink);font-weight:600}
.login{max-width:360px;margin:12vh auto;padding:0 16px}
.login form{display:grid;gap:12px}
.empty{padding:24px;color:var(--muted)}
@media (max-width:720px){.wide{display:none}.two{grid-template-columns:1fr}.who{margin-left:0}th,td{padding:6px 8px}}
`;

export function page(title: string, body: string, opts: { user?: User; tenant?: string; at?: string } = {}): string {
  const u = opts.user;
  const nav = u
    ? `<nav aria-label="Sections">${[
        ['/accounts', 'Accounts'],
        ['/transfers', 'Transfers'],
        ['/journal', 'Journal'],
        ...(u.role === 'admin' ? [['/reconciliation', 'Reconciliation']] : []),
      ]
        .map(([href, label]) => `<a href="${href}"${opts.at === href ? ' aria-current="page"' : ''}>${label}</a>`)
        .join('')}</nav>
      <div class="who"><span>${esc(u.display)}${u.role === 'admin' ? ', operator' : ''}</span>
      <form method="post" action="/logout"><button class="quiet">Sign out</button></form></div>`
    : '';
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>${esc(title)} – Quire</title><meta name="color-scheme" content="light dark"><style>${CSS}</style></head>
<body><header class="bar"><a class="mark" href="/">Quire<small>${esc(opts.tenant ?? '')}</small></a>${nav}</header>
<main>${body}</main></body></html>`;
}

export function flash(q: URLSearchParams): string {
  const done = q.get('done');
  const err = q.get('error');
  if (err) return `<p class="flash err" role="alert">${esc(err)}</p>`;
  if (done) return `<p class="flash" role="status">${esc(done)}</p>`;
  return '';
}

export interface AccountRow {
  ext: string;
  name: string;
  holders: string[] | null;
  currency: string;
  balance: number;
  held: number;
  status: string;
  kind: string;
}

export function accountsPage(user: User, rows: AccountRow[], q: URLSearchParams, key: string): string {
  const admin = user.role === 'admin';
  const body = rows.length
    ? rows
        .map(
          (a) => `<tr>
  <td><a href="/accounts/${encodeURIComponent(a.ext)}">${esc(a.name)}</a><div class="id">${esc(a.ext)}</div></td>
  <td class="wide">${esc((a.holders ?? []).join(', '))}</td>
  <td class="wide">${esc(a.currency)}</td>
  <td class="n wide">${money(a.balance)}</td>
  <td class="n wide">${a.held ? money(-a.held) : ''}</td>
  <td class="n">${money(a.balance - a.held, a.currency)}</td>
  <td>${a.kind === 'world' ? '<span class="tag">outside</span>' : a.status === 'frozen' ? '<span class="tag frozen">frozen</span>' : ''}</td>
  ${
    admin && a.kind === 'customer'
      ? `<td><form class="inline" method="post" action="/accounts/${encodeURIComponent(a.ext)}/status">
        <input type="hidden" name="status" value="${a.status === 'frozen' ? 'open' : 'frozen'}">
        <button class="${a.status === 'frozen' ? 'quiet' : 'danger'}">${a.status === 'frozen' ? 'Unfreeze' : 'Freeze'}</button></form></td>`
      : admin
        ? '<td></td>'
        : ''
  }
</tr>`,
        )
        .join('')
    : `<tr><td colspan="8" class="empty">No accounts yet.</td></tr>`;
  const forms = admin
    ? `<div class="two">
<section><h2>Deposit</h2>
<form class="grid" method="post" action="/deposits">
  <input type="hidden" name="key" value="${esc(key)}">
  <label>Into account<select name="to">${rows
    .filter((a) => a.kind === 'customer')
    .map((a) => `<option value="${esc(a.ext)}">${esc(a.ext)} (${esc(a.currency)})</option>`)
    .join('')}</select></label>
  <label>Amount<input class="amt" name="amount" inputmode="decimal" required placeholder="0.00"></label>
  <button>Deposit</button>
</form></section>
<section><h2>Open an account</h2>
<form class="grid" method="post" action="/accounts">
  <label>Account id<input name="ext" required pattern="[a-z0-9_\\-]{3,64}" placeholder="dev-savings"></label>
  <label>Name<input name="name" required></label>
  <label>Holders<input name="holders" required placeholder="dev, ada"></label>
  <label>Currency<select name="currency"><option>EUR</option><option>GBP</option><option>USD</option></select></label>
  <button>Open account</button>
</form></section></div>`
    : '';
  return `<h1>Accounts</h1>
<p class="lede">${admin ? 'Every account in this ledger. The outside accounts are where deposits come from, so each currency sums to zero.' : 'The accounts you hold, alone or jointly.'}</p>
${flash(q)}
<div class="sheet"><table>
<thead><tr><th>Account</th><th class="wide">Holders</th><th class="wide">Currency</th><th class="n wide">Balance</th><th class="n wide">On hold</th><th class="n">Available</th><th></th>${admin ? '<th></th>' : ''}</tr></thead>
<tbody>${body}</tbody></table></div>
${forms}`;
}

export interface EntryRow {
  entry: string;
  tx: string;
  account: string;
  currency: string;
  amount: number;
  kind: string;
  at: string;
}

export function entriesTable(rows: EntryRow[], opts: { account?: boolean } = {}): string {
  if (!rows.length) return '<div class="sheet"><p class="empty">No entries.</p></div>';
  return `<div class="sheet"><table>
<thead><tr><th class="wide">When</th><th>Movement</th>${opts.account === false ? '' : '<th>Account</th>'}<th class="wide">Kind</th><th class="n">Debit</th><th class="n">Credit</th></tr></thead>
<tbody>${rows
    .map(
      (
        e,
      ) => `<tr><td>${when(e.at)}</td><td class="id">${esc(e.tx)}</td>${opts.account === false ? '' : `<td><a href="/accounts/${encodeURIComponent(e.account)}">${esc(e.account)}</a></td>`}
<td class="wide">${esc(e.kind)}</td><td class="n">${e.amount < 0 ? money(-e.amount, e.currency) : ''}</td><td class="n">${e.amount > 0 ? money(e.amount, e.currency) : ''}</td></tr>`,
    )
    .join('')}</tbody></table></div>`;
}

export function accountPage(
  a: AccountRow,
  entries: EntryRow[],
  holds: { ref: string; amount: number; until: string; state: string }[],
): string {
  return `<h1>${esc(a.name)}</h1>
<p class="lede"><span class="id">${esc(a.ext)}</span>, held by ${esc((a.holders ?? []).join(' and ') || 'nobody')}${a.status === 'frozen' ? ' <span class="tag frozen">frozen</span>' : ''}</p>
<div class="facts"><span>Balance <b>${money(a.balance, a.currency)}</b></span><span>On hold <b>${money(a.held, a.currency)}</b></span><span>Available <b>${money(a.balance - a.held, a.currency)}</b></span></div>
${
  holds.length
    ? `<h2>Holds</h2><div class="sheet"><table><thead><tr><th>Hold</th><th>Until</th><th class="n">Amount</th></tr></thead><tbody>${holds
        .map((h) => `<tr><td class="id">${esc(h.ref)}</td><td>${when(h.until)}</td><td class="n">${money(h.amount, a.currency)}</td></tr>`)
        .join('')}</tbody></table></div>`
    : ''
}
<h2>Entries</h2>${entriesTable(entries, { account: false })}`;
}

export interface TransferRow {
  ref: string;
  kind: string;
  src: string;
  dst: string;
  currency: string;
  amount: number;
  refunded: number;
  of: string | null;
  memo: string;
  at: string;
}

export function transfersPage(
  user: User,
  mine: AccountRow[],
  rows: TransferRow[],
  refundable: Set<string>,
  q: URLSearchParams,
  key: string,
): string {
  const from = mine.filter((a) => a.kind === 'customer');
  const list = rows.length
    ? rows
        .map((t) => {
          const left = t.amount - t.refunded;
          const can = refundable.has(t.ref) && (t.kind === 'transfer' || t.kind === 'capture') && left > 0;
          return `<tr><td class="wide">${when(t.at)}</td><td class="wrap">${esc(t.memo) || '<span class="id">No reference</span>'}<div class="id">${esc(t.ref)}${t.of ? `, of ${esc(t.of)}` : ''}</div></td>
<td>${esc(t.src)}</td><td>${esc(t.dst)}</td><td class="wide">${esc(t.kind)}</td>
<td class="n">${money(t.amount, t.currency)}</td><td class="n wide">${t.refunded ? money(-t.refunded) : ''}</td>
<td>${
            can
              ? `<form class="inline" method="post" action="/refunds"><input type="hidden" name="key" value="${esc(key)}-${esc(t.ref)}"><input type="hidden" name="of" value="${esc(t.ref)}">
<input class="amt refund" name="amount" value="${esc(formatAmount(left).replace(/,/g, ''))}" aria-label="Refund amount for ${esc(t.ref)}">
<button class="quiet">Refund</button></form>`
              : ''
          }</td></tr>`;
        })
        .join('')
    : '<tr><td colspan="8" class="empty">No movements yet. Make the first one above.</td></tr>';
  return `<h1>Transfers</h1>
<p class="lede">A transfer moves money between two accounts in the same currency. It lands whole or not at all, and sending this form twice makes it once.</p>
${flash(q)}
<form class="grid" method="post" action="/transfers">
  <input type="hidden" name="key" value="${esc(key)}">
  <label>From<select name="from" required>${from.map((a) => `<option value="${esc(a.ext)}">${esc(a.ext)}, ${esc(symbol(a.currency))}${formatAmount(a.balance - a.held)}</option>`).join('')}</select></label>
  <label>To account<input name="to" required placeholder="cleo-studio" autocomplete="off"></label>
  <label>Amount<input class="amt" name="amount" required inputmode="decimal" placeholder="0.00"></label>
  <label>Reference<input name="memo" maxlength="140" placeholder="Invoice 2231"></label>
  <button>Send transfer</button>
</form>
<h2>Recent movements</h2>
<div class="sheet"><table><thead><tr><th class="wide">When</th><th>Movement</th><th>From</th><th>To</th><th class="wide">Kind</th><th class="n">Amount</th><th class="n wide">Refunded</th><th>Refund</th></tr></thead>
<tbody>${list}</tbody></table></div>`;
}

export function journalPage(rows: EntryRow[], account: string | null): string {
  return `<h1>Journal</h1>
<p class="lede">Every movement is two entries that sum to zero. Entries are only ever added: a refund or a correction is a new movement, never an edit.</p>
<form method="get" action="/journal" class="grid" style="max-width:520px;margin-bottom:16px">
  <label>Account<input name="account" value="${esc(account ?? '')}" placeholder="All accounts"></label><button>Show</button>
</form>
${entriesTable(rows)}`;
}

export function reconciliationPage(
  r: Report,
  sink: { lines: number; entries: number; matches: boolean | null; behind: number | null },
): string {
  const rows = r.currencies
    .map(
      (
        c,
      ) => `<tr><td>${esc(c.currency)}</td><td class="n">${money(c.customers)}</td><td class="n">${money(c.customerEntries)}</td><td class="n wide">${money(c.outside)}</td>
<td class="n">${money(c.customers - c.customerEntries)}</td><td class="n wide">${money(c.held)}</td><td class="n wide">${money(c.holds)}</td></tr>`,
    )
    .join('');
  // Each currency stands apart: what is totalled is how many are out.
  const off = r.currencies.filter((c) => c.customers !== c.customerEntries || c.balances !== 0 || c.journal !== 0).length;
  const issues = [
    ...r.drift.map(
      (d) => `<li><b>${esc(d.account)}</b>: balance ${formatAmount(d.balance)}, entries sum to ${formatAmount(d.journal)}</li>`,
    ),
    ...r.heldDrift.map(
      (d) => `<li><b>${esc(d.account)}</b>: ${formatAmount(d.held)} on hold, live holds sum to ${formatAmount(d.holds)}</li>`,
    ),
    ...r.negative.map((a) => `<li><b>${esc(a)}</b> is below zero</li>`),
    ...r.overRefunded.map((t) => `<li><b>${esc(t)}</b> was refunded past its amount</li>`),
  ];
  return `<h1>Reconciliation</h1>
<p class="lede">Balances against the journal, read in one snapshot: no write lands between the reads. What customer accounts hold must equal the sum of their entries, and the outside account must hold the opposite.</p>
<div class="verdict"><strong class="${r.ok ? 'ok' : 'bad'}">${r.ok ? 'Balanced' : 'Out of balance'}</strong>
<span class="id">at change ${esc(r.seq ?? '')}, read in ${r.ms.toFixed(1)} ms</span></div>
<div class="facts"><span>Accounts <b>${r.accounts}</b></span><span>Entries <b>${r.entries.toLocaleString('en-GB')}</b></span></div>
<div class="sheet"><table>
<thead><tr><th>Currency</th><th class="n">In accounts</th><th class="n">Their entries</th><th class="n wide">Outside</th><th class="n">Difference</th><th class="n wide">On hold</th><th class="n wide">Live holds</th></tr></thead>
<tbody>${rows}</tbody>
<tfoot><tr><td>Currencies out of balance</td><td></td><td></td><td class="wide"></td><td class="n">${off}</td><td class="wide"></td><td class="wide"></td></tr></tfoot>
</table></div>
${issues.length ? `<h2>Differences</h2><ul>${issues.join('')}</ul>` : ''}
<h2>Journal sink</h2>
<p class="lede">The change stream copies every entry to a file outside the database, at least once, keyed by the entry's id.</p>
<div class="facts"><span>Lines <b>${sink.lines.toLocaleString('en-GB')}</b></span><span>Distinct entries <b>${sink.entries.toLocaleString('en-GB')}</b></span>
<span>Writes behind <b>${sink.behind ?? 'not running'}</b></span>
<span>${sink.matches === null ? 'Not compared' : sink.matches ? '<span class="ok">Matches the journal</span>' : '<span class="bad">Differs from the journal</span>'}</span></div>`;
}

export function loginPage(error?: string): string {
  return `<div class="login"><h1>Sign in</h1><p class="lede">Operators and account holders sign in here. Each sees only what their own token can read.</p>
${error ? `<p class="flash err" role="alert">${esc(error)}</p>` : ''}
<form method="post" action="/login" class="sheet" style="padding:16px">
<label>Name<input name="name" autocomplete="username" required autofocus></label>
<label>Password<input name="password" type="password" autocomplete="current-password" required></label>
<button>Sign in</button></form></div>`;
}

export function notFound(): string {
  return `<h1>Not found</h1><p class="lede">There is nothing here you can see. <a href="/accounts">Back to accounts</a>.</p>`;
}
