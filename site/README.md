# The fenecdb website

Dusk in the dune field. The fennec is a desert fox -- small, tough, and all
ears -- so the site is a night sky over lit dunes, and the fox at its top
hears every write. Everything that moves is the real engine running, a real
measurement being drawn, or what the database does, drawn plainly.

The generator (`build.py`) is standard library only, in keeping with the repo's
zero-dependency rule; it exists so that page chrome lives in one place instead
of being copied into fifteen files and drifting.

JS and CSS go through `esbuild` when it is on `PATH`, and ship as written when
it is not — the generator keeps no dependency of its own, it just uses the tool
if it finds it. Worth having: the four assets are 38 KB gzipped as written and
23 KB minified. `.github/workflows/site.yml` installs a pinned version, so the
deploy is always the minified one.

```bash
make site          # -> site/dist
make site-serve    # build, then http://localhost:8788
make site-deploy   # build, then wrangler deploy
```

All three depend on `make wasm`: the console on the home page runs the real
engine, so `build.py` copies `web/fenec.js` and `web/fenec.wasm` into the
output. Without them the console falls back to the published measurements and
says so on screen.

Because it has the module in hand, `build.py` also checks the numbers the prose
puts on it. The module's size is written into eight files and had drifted by
13 KB across all of them before this existed; `CLAIMS` at the top of the
generator lists every place, with the pattern that finds it. A mismatch is a
warning locally — a rebuild moving the module by forty bytes should not stop
you working — and an error when `CI` is set, which is the build that would ship
the wrong number. Reword one of those sentences and the check says it could not
find its pattern: update the entry, do not delete it.

## Deployment

Cloudflare Workers, assets-only — there is no `main` in `wrangler.jsonc`, so
every request is answered from `site/dist` by the static-asset router. Same
shape as `apps/site` in the backlex repo, including its two hard-won rules:

- **Routes are not declared in `wrangler.jsonc`.** A deploy token with Workers
  Scripts permissions but without *Zone › Workers Routes* uploads the worker and
  the assets fine and then fails reconciling the route (Authentication error
  10000). The routes for `fenecdb.com` are stable, so they stay
  dashboard-managed and CI deploys cleanly. To move them into the config, grant
  the token *Zone › Workers Routes › Edit* on the zone.
- **The apex needs its own proxied A/AAAA record.** A `*.fenecdb.com` wildcard
  does not cover `fenecdb.com`.

`.github/workflows/site.yml` builds the wasm and deploys on a push to `main`
that touches `site/`, `wrangler.jsonc` or the crates the module is built from.
It needs two repository secrets: `CLOUDFLARE_API_TOKEN` (the *Edit Cloudflare
Workers* template) and `CLOUDFLARE_ACCOUNT_ID`. Neither is committed — no
`account_id` lives in the config, so `wrangler` resolves it from the login
session locally and from the environment in CI.

Locally, `wrangler login` once, then `make site-deploy`.

`site/dist/_headers` is written by `build.py` on every build: security headers
for everything, and a year of immutable caching for every content-hashed asset.
That now includes the engine — the site loads `fenec.<hash>.js` and
`fenec.<hash>.wasm`, so a returning visitor pays no revalidation round trip for
the two largest files on the page.

Both also ship under their plain names, because `./fenec.js` is what the docs
tell people to import and those links have to keep resolving. Only the plain
pair revalidates: a stale module under a stable name would silently be the
wrong engine. They are deliberately not run through the page-wide asset rename
either, or the `./fenec.js` inside every docs code sample would be rewritten
into a hashed name that means nothing to a reader.

## Layout

```
site/
  build.py        the generator
  template.html   the page shell; {{placeholders}} are filled per page
  styles.css      the whole design system
  site.js         the sky, the live console, the scenes, the screencast, docs navigation
  fennec.js       the mark: one table of points and edges
  motion.js       each section's scenes, drawn from their time alone
  mark.svg        the mark, written by `node site/fennec.js`: the favicon
  mark-detail.svg the same with its running light: header and footer
  engine-worker.js  the engine off the main thread: home console, playground
  content/
    index.html    the home page
    404.html      served by not_found_handling
    docs/*.html   one fragment per docs page
  dist/           generated output, gitignored
```

## Adding a docs page

1. Write `content/docs/<name>.html`, starting with the metadata comment:

   ```html
   <!--
   title: Thing — fenecdb
   description: One sentence, used for the meta description and link previews.
   -->

   <h1>Thing</h1>
   <p class="kicker">One or two sentences under the title.</p>
   ```

2. Add it to `NAV` in `build.py`, under the group it belongs to. That list
   drives the sidebar order and the previous/next links at the foot of a page.

Every `<h2>` and `<h3>` gets an id and a table-of-contents entry automatically;
write your own `id="..."` when another page already links to it.

## Conventions

- Code goes in `<pre data-lang="fenecql|js|rust|bash|json|http|text">`, with an
  optional `data-label="filename"`. Write `<` and `>` raw; the generator
  escapes and highlights. `data-bare` highlights in place without the panel —
  that is how the typed headline on the home page is set.
- Tables: wrap in `<div class="tbl">` and mark numeric cells `class="n"` so
  they align right and never wrap across two lines.
- `<div class="note">` for something worth knowing, `<div class="note trap">`
  for something that will bite. Both are cheap, so neither should be common.
- Add `class="rise"` to reveal a block on scroll. Anything inside `.strata`
  reveals itself.
- Every number needs its method nearby. The repo is careful about this and the
  site has to be too, or it stops being trustworthy.

## The mark

The logo is a fennec's head drawn as a graph -- points joined by edges, as an
index over vectors is -- in amber lines, with no fill, no dots at the joints
and no name beside it. As the page opens a teal light runs out from the tip
of the right ear through every edge, wave by wave, the way a search spreads
through a graph, and is gone in under a second; hovering the mark runs it
again. Each edge is a `pathLength="1"` path with a CSS `stroke-dashoffset`
animation and its own delay (`FLOW` in `fennec.js`), so it costs no script,
and it is hidden under reduced motion.

`fennec.js` is the one table it is drawn from: `node site/fennec.js` writes
`mark-detail.svg` (with the light; the header and the footer inline it) and
`mark.svg` (thicker lines, no light: the favicon), and `motion.js` draws the
same lines, and runs the same light once, on its canvases.

## The hero

The fennec hearing everything: the mark's lines in the middle, rings
travelling out from its ears, and a phone, a browser window, a server and
three tenant files around it, each lighting as a ring reaches it -- a write
landing everywhere it is read. It is an inline SVG in `content/index.html`
and CSS alone, one 8 s cycle: a ring grows 250 px in 5.6 s, and each glyph
lights at its `--hit`, its distance from the nearer ear at that pace. The
mark's path comes from `fennec.js` through `mark.svg` (`{{mark_lines}}` in
the page, filled by `build.py`), so it cannot drift from the logo. Only
transform and opacity move; `site.js` pauses it while the hero is off
screen, and under reduced motion it is a still frame with the rings drawn.
It replaced a WebGL cloud of documents gathering into an HNSW index, which
said "vector database" before a word was read.

## Moving pictures

Each of the home page's six sections tells its story as a scene, not as
code: a canvas between its heading and its detail, the measured numbers in
the text beside it. A write redrawing the three screens that read it while a
fourth is not run (`state`); a write made offline, sent once and drawn on
another screen (`flow`); sixteen writers through one lock into one file, an
fsync covering several writes, readers going on beside them (`writers`);
requests straight to the server and its mapped file, the cache crossed out
(`traffic`); tenants moved and failed over onto their copies (`scale`); and a
question finding its documents by meaning and by its words, the two lists
fused (`search`). What moves is the mark's own light, the teal with its glow,
so the scenes and the logo read as one.

`motion.js` draws any moment of a scene from the scene's time alone, so a
section loops its scene while in view and stops when it is not; under
reduced motion each shows its last frame, which says everything the scene
does. A scene has two stages, 1280 wide and 480 wide (`stage`): a phone
draws the narrow one, the same story laid out taller, since the wide stage
scaled to a phone turned its words to specks. It loads as the first scene
comes near. Every number in it is one the docs measure; the tasks, the
documents and their places are only examples.

## The screencast

The home page's "A real session, recorded" plays a session as text: commands typed, output
a line at a time, chapters to jump between, paused while off screen. The
transcript is the `<ol class="cast-script">` in `content/index.html`, and every
line of output in it came from a real `fenec-server`, `curl` and the `fenecdb` Python client --
re-record it rather than edit an output by hand. Without script the transcript
is the page.

## Motion, and what it costs

`prefers-reduced-motion` is honoured throughout: the sky canvases stop, the
hero's rings are drawn still, the mark's light is hidden, the
headline appears whole, each scene shows its last frame, the screencast
waits to be played, and nothing translates.

Two things were measured and fixed, and are worth not reintroducing:

- **A full-viewport `mix-blend-mode` overlay froze the renderer.** Blending a
  fixed layer over two animating canvases forces a whole-page CPU composite
  every frame. The grain is painted into the page background instead.
- **`filter: blur()` on a large animated element re-rasterises it every frame.**
  The horizon glow is a soft radial gradient with no filter.

Both canvases stop entirely once the hero scrolls out of view, and while the tab
is hidden.
