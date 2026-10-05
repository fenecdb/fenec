// The one stylesheet, inlined into every page (a request less before the
// first paint) and allowed by its hash in the content security policy.
//
// Kestrel's look: a field-station console. Cool survey-paper ground, slate
// ink, the kestrel's blue-grey for data and its tawny ochre for what is
// live. One family, Archivo, whose width axis does the work a second face
// would: figures condensed and heavy, prose at its normal width.
//
// `font-display: optional`: the face is used when it is there by the first
// paint -- preloaded, and cached after the first visit -- and never swapped
// in after it. Swapped in on a slow link, the narrower figures moved every
// table and chart under them: Lighthouse measured a layout shift of 0.25
// to 0.36 on the 90-day dashboard and the market page.
const widths = Array.from({ length: 21 }, (_, i) => `.w${i * 5}{width:${i * 5}%}`).join('');
// Retention's shades: h1 to h8 up to `--heat` of the slate, with the ink
// on them, and h9 (week 0) the slate itself with the paper on it. `--heat`
// is as far as the ink keeps 4.5:1 on the mix in each theme: 62% on paper,
// 45% in the dark, where the slate is the lighter of the two.
const shades = Array.from({ length: 10 }, (_, i) =>
  i === 9 ? '.h9{background:var(--slate)}' : `.h${i}{background:color-mix(in srgb,var(--slate) calc(var(--heat) * ${i / 8}),var(--paper))}`,
).join('');

export const CSS = `
@font-face{font-family:Archivo;font-style:normal;font-display:optional;font-weight:100 900;font-stretch:62% 125%;src:url(/fonts/archivo.woff2) format('woff2')}
:root{--paper:#f1f3ef;--raise:#e6eae5;--ink:#1b2833;--muted:#52606b;--rule:#ccd4d0;--slate:#46678a;--slate-2:#9fb5ca;--ochre:#b47f1c;--ochre-ink:#7d5609;--up:#2c6e4d;--down:#a8382c;--heat:62%;color-scheme:light}
@media (prefers-color-scheme:dark){:root:not([data-theme=light]){--paper:#121a21;--raise:#19242d;--ink:#e0e6e9;--muted:#9aa8b2;--rule:#2b3843;--slate:#86a8c8;--slate-2:#3d5873;--ochre:#d9a645;--ochre-ink:#e5b862;--up:#62b88a;--down:#e4796a;--heat:45%;color-scheme:dark}}
:root[data-theme=dark]{--paper:#121a21;--raise:#19242d;--ink:#e0e6e9;--muted:#9aa8b2;--rule:#2b3843;--slate:#86a8c8;--slate-2:#3d5873;--ochre:#d9a645;--ochre-ink:#e5b862;--up:#62b88a;--down:#e4796a;--heat:45%;color-scheme:dark}
*{box-sizing:border-box}
html{-webkit-text-size-adjust:100%}
body{margin:0;background:var(--paper);color:var(--ink);font:400 15px/1.5 Archivo,"Arial Narrow",system-ui,sans-serif;font-variant-numeric:tabular-nums}
a{color:inherit}
a:focus-visible,button:focus-visible,input:focus-visible,select:focus-visible,summary:focus-visible{outline:2px solid var(--ochre);outline-offset:2px}
.sr{position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap}
.top{display:flex;flex-wrap:wrap;align-items:center;gap:12px 28px;padding:12px 24px;border-bottom:1px solid var(--rule)}
.brand{display:flex;align-items:center;gap:8px;font-weight:750;font-stretch:78%;font-size:21px;letter-spacing:.01em;text-decoration:none}
.brand svg,.mark{width:26px;height:26px;fill:var(--slate)}.mark{width:48px;height:48px}.eye{fill:var(--paper)}
.sites{display:flex;gap:4px;flex-wrap:wrap}
.sites a,.seg a{padding:5px 10px;border-radius:3px;text-decoration:none;color:var(--muted)}
.sites a[aria-current]{color:var(--ink);font-weight:650;background:var(--raise)}
.seg{display:flex;border:1px solid var(--rule);border-radius:4px;overflow:hidden}
.seg a{border-radius:0;border-left:1px solid var(--rule);font-stretch:88%}
.seg a:first-child{border-left:0}
.seg a[aria-current]{background:var(--ink);color:var(--paper);font-weight:650}
.spacer{flex:1}
.who{display:flex;align-items:center;gap:10px;color:var(--muted);font-size:13px}
button,.btn{font:inherit;font-size:14px;padding:6px 12px;border:1px solid var(--rule);border-radius:3px;background:var(--raise);color:var(--ink);cursor:pointer}
button.primary{background:var(--ink);color:var(--paper);border-color:var(--ink);font-weight:650}
.pulse{display:grid;grid-template-columns:auto auto 1fr;align-items:end;gap:6px 28px;padding:20px 24px 18px;border-bottom:1px solid var(--rule)}
.now{display:flex;align-items:baseline;gap:10px}
.big{font-size:56px;line-height:.9;font-weight:760;font-stretch:66%}
.now .lbl{color:var(--muted);max-width:14em;line-height:1.25}
.live{display:inline-block;width:9px;height:9px;border-radius:50%;background:var(--ochre);margin-right:6px;vertical-align:1px}
.live.on{animation:beat 2.4s ease-out infinite}
@keyframes beat{0%{box-shadow:0 0 0 0 color-mix(in srgb,var(--ochre) 70%,transparent)}70%{box-shadow:0 0 0 9px transparent}100%{box-shadow:0 0 0 0 transparent}}
@media (prefers-reduced-motion:reduce){.live.on{animation:none}}
.spark{display:block;width:220px;height:44px}
.spark .col{fill:var(--slate-2)}.spark .now{fill:var(--ochre)}
.minor{color:var(--muted);font-size:13px}
.layout{display:grid;grid-template-columns:232px minmax(0,1fr);gap:36px;padding:24px 24px 48px}
.filters summary{font-weight:650;font-stretch:85%;font-size:17px;cursor:pointer;margin-bottom:8px}
.filters fieldset{border:0;padding:0;margin:0 0 20px}
.filters legend{font-weight:650;font-size:13px;color:var(--muted);margin-bottom:4px;padding:0}
.opt{position:relative;display:flex;align-items:center;gap:8px;padding:3px 6px;font-size:14px;border-radius:2px}
.opt input{margin:0;accent-color:var(--slate);position:relative;z-index:1}
.opt .v{flex:1;position:relative;z-index:1}.opt .c{position:relative;z-index:1;color:var(--muted);font-size:13px}
.opt .bar{position:absolute;left:0;top:2px;bottom:2px;background:var(--raise);border-radius:2px}
.opt:has(input:checked) .bar{background:color-mix(in srgb,var(--slate) 22%,var(--paper))}
.filters .apply{margin-top:4px}
.js .filters .apply{display:none}
.head{display:flex;flex-wrap:wrap;align-items:baseline;gap:6px 18px;margin-bottom:6px}
h1,h2{font-weight:680;font-stretch:84%;margin:0;line-height:1.15}
h1{font-size:26px}h2{font-size:19px}
.src{font-size:13px;color:var(--muted);display:inline-flex;align-items:center;gap:6px}
.src::before{content:"";width:8px;height:8px;border-radius:2px;background:var(--slate)}
.src.raw::before{background:var(--ochre)}
.totals{display:flex;flex-wrap:wrap;gap:8px 40px;margin:14px 0 6px}
.totals div{display:flex;flex-direction:column}
.totals dt{color:var(--muted);font-size:13px;order:2}
.totals dd{margin:0;font-size:38px;line-height:1;font-weight:740;font-stretch:68%}
.key{display:flex;gap:18px;font-size:13px;color:var(--muted);margin:4px 0 10px}
.key span::before{content:"";display:inline-block;width:12px;height:10px;margin-right:6px;vertical-align:-1px;background:var(--slate-2)}
.key .vis::before{height:2px;vertical-align:3px;background:var(--ochre)}
section{margin-top:40px}
figure{margin:0}
.area{display:flex;gap:8px;height:240px}
.tc .area{flex-direction:row}
.plot{flex:1;min-width:0;height:100%;overflow:visible}
.plot .grid{stroke:var(--rule);stroke-width:1;vector-effect:non-scaling-stroke}
.plot .col{fill:var(--slate-2)}
.plot g:hover .col{fill:var(--slate)}
.plot .hit{fill:transparent}
.plot .line{fill:none;stroke:var(--ochre);stroke-width:2.5;vector-effect:non-scaling-stroke;stroke-linejoin:round}
.ya{display:flex;flex-direction:column;justify-content:space-between;width:40px;font-size:12px;color:var(--muted);text-align:right;margin:-.6em 0}
.cc .ya{text-align:left;width:56px}
.xa{display:flex;margin:6px 0 0 48px;font-size:12px;color:var(--muted)}
.cc .xa{margin:6px 64px 0 0}
.xa span{flex:1 1 0;min-width:0;white-space:nowrap;overflow:visible}
.cols{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,340px),1fr));gap:12px 40px}
table{border-collapse:collapse;width:100%;font-size:14px}
th{font-weight:600;text-align:left}
thead th{font-size:12px;color:var(--muted);font-weight:600;padding:0 8px 6px;border-bottom:1px solid var(--rule)}
td,tbody th{padding:6px 8px;border-bottom:1px solid var(--rule)}
.num{text-align:right}
.rank td:first-child{position:relative;max-width:0;width:60%}
.rank .name{position:relative;z-index:1;display:block;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.rank .bar{position:absolute;left:0;top:4px;bottom:4px;background:var(--raise);border-radius:2px}
.funnel{list-style:none;margin:12px 0 0;padding:0;display:grid;gap:14px}
.funnel .row{display:flex;justify-content:space-between;align-items:baseline;gap:12px}
.funnel .n{font-size:26px;font-weight:740;font-stretch:70%}
.funnel .track{height:12px;background:var(--raise);border-radius:2px;margin-top:4px}
.funnel .fill{height:100%;background:var(--slate);border-radius:2px}
.funnel .drop{font-size:13px;color:var(--muted)}
.ret{font-size:13px}
.ret td{text-align:center;padding:6px 4px;border-bottom:2px solid var(--paper)}
.ret tbody th{font-weight:500;white-space:nowrap;padding-right:12px}
.ret caption{caption-side:bottom;text-align:left;color:var(--muted);font-size:13px;padding-top:8px}
.ret .h9{color:var(--paper)}.ret .part{font-style:italic;outline:1px dashed var(--rule);outline-offset:-3px}
.scroll{overflow-x:auto}
.empty{color:var(--muted);padding:8px 0}
.note{color:var(--muted);font-size:13px;max-width:70ch}
footer{padding:20px 24px 40px;border-top:1px solid var(--rule);color:var(--muted);font-size:13px}
.layout.mk{grid-template-columns:minmax(0,1fr);gap:8px}.mk .area{height:300px}
.mk .vol{height:64px;margin-top:6px}
.vols{flex:1;min-width:0;height:100%}
.up{color:var(--up)}.down{color:var(--down)}
.plot .up .body,.plot .up .wick{fill:var(--up);stroke:var(--up)}
.plot .down .body,.plot .down .wick{fill:var(--down);stroke:var(--down)}
.plot .wick{stroke-width:1.2;vector-effect:non-scaling-stroke}
.vols .up{fill:color-mix(in srgb,var(--up) 45%,var(--paper))}.vols .down{fill:color-mix(in srgb,var(--down) 45%,var(--paper))}
.plot .vwap{fill:none;stroke:var(--ochre);stroke-width:2;vector-effect:non-scaling-stroke;stroke-dasharray:6 3}
.quotes td:first-child a{font-weight:650;text-decoration:none}
.quotes tr[aria-current] td{background:var(--raise)}
.quotes .flash{animation:flash 1s ease-out}
@keyframes flash{from{background:color-mix(in srgb,var(--ochre) 30%,transparent)}to{background:transparent}}
@media (prefers-reduced-motion:reduce){.quotes .flash{animation:none}}
.controls{display:flex;flex-wrap:wrap;gap:10px 18px;align-items:center;margin:10px 0 14px}
.signin{max-width:460px;margin:12vh auto 0;padding:0 16px}
.signin h1{font-size:54px;font-stretch:66%;font-weight:760;line-height:1;margin:18px 0 8px}
.signin p{max-width:42ch}
.signin form{display:grid;gap:12px;margin:24px 0}
.signin label{display:grid;gap:4px;font-size:14px;color:var(--muted)}
.signin input{font:inherit;padding:9px 10px;border:1px solid var(--rule);border-radius:3px;background:var(--paper);color:var(--ink)}
.error{color:var(--down)}
@media (max-width:760px){
.top{padding:10px 16px;gap:10px 16px}.seg a{padding:5px 8px}
.pulse{grid-template-columns:1fr auto;padding:16px}.pulse .minor{grid-column:1/-1}
.spark{width:140px}
.big{font-size:46px}
.layout{grid-template-columns:minmax(0,1fr);gap:8px;padding:16px 16px 40px}
.area{height:200px}.mk .area{height:240px}
.totals dd{font-size:32px}
.ya{width:34px}.xa{margin-left:42px}.cc .xa{margin-right:52px}.cc .ya{width:46px}
.hide-s{display:none}.xa .alt{visibility:hidden}
}
${widths}${shades}
`
  .replace(/\n/g, '')
  .trim();
